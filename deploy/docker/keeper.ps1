param(
    [Parameter(Position=0)][ValidateSet('init','install','build','image','keys','up','stop','down','restart','status','health','sweep','logs','config','migrate')][string]$Action='status',
    # install and keys: the transaction key file. image: the image digest or loaded image ID.
    [Parameter(Position=1)][string]$TransactionKey,
    [Parameter(Position=2)][string]$VrfKey,
    [string]$FromDb,
    [ValidateSet('prepare','apply','resume')][string]$MigrationMode,
    [string]$SweepAmount,
    [string]$SweepKeep,
    [switch]$SweepCancel
)
$ErrorActionPreference='Stop'
$composeFile=Join-Path $PSScriptRoot 'compose.yaml'
# KEEPER_INSTANCE (process environment only) selects one of several keepers on this host. Unset keeps the single
# keeper exactly as before: project d20dao, keeper.env, state volume d20dao-state-v1, image d20dao-keeper:local.
$instance=[Environment]::GetEnvironmentVariable('KEEPER_INSTANCE')
if (!$instance) {
    $project='d20dao'
    $configFile=Join-Path $PSScriptRoot 'keeper.env'
    $stateVolume='d20dao-state-v1'
    $defaultKeys='d20dao-keys-v1'
    $imageName='d20dao-keeper:local'
} else {
    if ($instance -cnotmatch '^[a-z0-9][a-z0-9-]{0,39}$' -or $instance -ceq 'local' -or $instance -ceq 'rollback') {
        throw 'KEEPER_INSTANCE must use at most 40 lowercase letters, digits and hyphens, start with a letter or digit, and not be local or rollback.'
    }
    $project="d20dao-$instance"
    $configFile=Join-Path $PSScriptRoot "instances/$instance/keeper.env"
    $stateVolume="d20dao-$instance-state"
    $defaultKeys="d20dao-$instance-keys"
    $imageName="d20dao-keeper:$instance"
}
$configName=if ($instance) { "instances/$instance/keeper.env" } else { 'keeper.env' }
# Read one setting without executing the file.
function Get-Setting([string]$File,[string]$Name) {
    $settings=@(Get-Content -LiteralPath $File | Where-Object { $_ -cmatch "^\s*(export\s+)?$Name\s*=" })
    if ($settings.Count -gt 1) { throw "Duplicate $Name setting." }
    if ($settings.Count -eq 1) { return ($settings[0] -replace '^[^=]*=', '') }
    return $null
}
$configuredKeys=$null
$threads=$null
if (Test-Path -LiteralPath $configFile) {
    $configuredKeys=Get-Setting $configFile 'KEYS_VOLUME'
    $threads=Get-Setting $configFile 'TOKIO_WORKER_THREADS'
}
$environmentKeys=[Environment]::GetEnvironmentVariable('KEYS_VOLUME')
if ($null -eq $environmentKeys) { $keysVolume=$configuredKeys }
elseif (!$instance -or $environmentKeys -ceq $(if ($configuredKeys) { $configuredKeys } else { $defaultKeys })) { $keysVolume=$environmentKeys }
else { throw "Set KEYS_VOLUME in $configName, not in the process environment." }
if (!$keysVolume) { $keysVolume=$defaultKeys }
if ($keysVolume -cnotmatch '^[a-zA-Z0-9][a-zA-Z0-9_.-]*$') { throw 'KEYS_VOLUME must be an unquoted safe Docker volume name.' }
if ($keysVolume -ceq 'd20dao-state-v1' -or $keysVolume -clike 'd20dao-*-state') { throw 'Keys and state must use different volumes.' }
if ($instance -and $keysVolume -ceq 'd20dao-keys-v1') { throw 'd20dao-keys-v1 is the single keeper''s keys volume; give the instance its own KEYS_VOLUME.' }
if (!$threads) { $threads='4' }
if ($threads -cnotmatch '^([1-9]|1[0-6])$') { throw 'TOKIO_WORKER_THREADS must be an unquoted integer from 1 to 16.' }
# Compose reads these for the selected instance; they are set only for the Compose call and then restored.
$composeSettings=@{KEYS_VOLUME=$keysVolume;KEEPER_STATE_VOLUME=$stateVolume;KEEPER_ENV_FILE=$configFile;KEEPER_IMAGE=$imageName;TOKIO_WORKER_THREADS=$threads}
function Invoke-Docker([string[]]$DockerArgs) {
    & docker @DockerArgs
    if ($LASTEXITCODE -ne 0) { throw "Docker command failed ($LASTEXITCODE)" }
}
function Invoke-Compose([string[]]$ComposeArgs) {
    $saved=@{}
    foreach ($name in $composeSettings.Keys) {
        $saved[$name]=[Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name,$composeSettings[$name])
    }
    try { Invoke-Docker (@('compose','--project-name',$project,'--env-file',$configFile,'--file',$composeFile)+$ComposeArgs) }
    finally { foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name,$saved[$name]) } }
}
function Test-Image([string]$Reference) {
    $ErrorActionPreference='Continue'
    & docker image inspect $Reference 2>&1 | Out-Null
    return $LASTEXITCODE -eq 0
}
function Assert-Image {
    if (Test-Image $imageName) { return }
    if ($instance) { throw "Image $imageName is missing; select one with ./keeper.ps1 image <image@sha256:digest | sha256:image-id>." }
    throw "Image $imageName is missing; run build first."
}
# keys and migrate never run beside a container that uses the volume, such as the running keeper.
function Assert-Stopped([string]$VolumeName,[string]$Kind,[string]$Before) {
    $running=& docker ps -q --filter "volume=$VolumeName"
    if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect running containers.' }
    if ($running) { throw "Stop containers using the $Kind volume before $Before." }
}
function Ensure-Volumes {
    foreach ($volumeName in @($stateVolume,$keysVolume)) {
        Invoke-Docker @('volume','create','--label','io.d20dao.component=keeper',$volumeName) | Out-Null
    }
}
function Get-Scope([string]$File) {
    try { $chain=Get-Setting $File 'CHAIN_ID'; $coordinator=Get-Setting $File 'COORDINATOR_ADDRESS' } catch { return $null }
    if (!$chain -or !$coordinator) { return $null }
    return (($chain -replace "['""]",'') + ':' + ($coordinator -replace "['""]",'').ToLowerInvariant())
}
# One keeper per volume and per chain coordinator on a host; scope locks live in each instance's own state volume.
function Assert-Exclusive {
    foreach ($volumeName in @($stateVolume,$keysVolume)) {
        $labels=@(& docker ps --filter "volume=$volumeName" --format '{{.Labels}}')
        if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect running containers.' }
        foreach ($line in $labels) {
            $owner=@($line -split ',' | Where-Object { $_ -clike 'com.docker.compose.project=*' } | ForEach-Object { $_.Substring(27) })
            if ($owner.Count -ne 1 -or $owner[0] -cne $project) { throw "Volume $volumeName is in use by a container outside project $project." }
        }
    }
    $scope=Get-Scope $configFile
    if (!$scope) { return }
    $others=@(Join-Path $PSScriptRoot 'keeper.env')
    $instances=Join-Path $PSScriptRoot 'instances'
    if (Test-Path -LiteralPath $instances) { $others+=@(Get-ChildItem -LiteralPath $instances -Directory | ForEach-Object { Join-Path $_.FullName 'keeper.env' }) }
    foreach ($other in $others) {
        if (!(Test-Path -LiteralPath $other) -or [IO.Path]::GetFullPath($other) -eq [IO.Path]::GetFullPath($configFile)) { continue }
        if ((Get-Scope $other) -ceq $scope) { throw "$other configures the same chain and coordinator; run one keeper per coordinator on a host." }
    }
}
if ($Action -eq 'init') {
    if (Test-Path -LiteralPath $configFile) { Write-Output "Existing $configName preserved." }
    elseif (!$instance) { Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'keeper.env.example') -Destination $configFile; Write-Output "Created $configName; configure deployment before up." }
    else {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $configFile) | Out-Null
        $example=[IO.File]::ReadAllText((Join-Path $PSScriptRoot 'keeper.env.example'))
        $text=[Text.RegularExpressions.Regex]::Replace($example,'(?m)^KEYS_VOLUME=.*$',"KEYS_VOLUME=$defaultKeys")
        [IO.File]::WriteAllText($configFile,$text,(New-Object Text.UTF8Encoding $false))
        Write-Output "Created $configName; configure deployment before up."
    }
    exit 0
}
if ($Action -eq 'build') {
    if ($instance) { throw 'A named instance never builds on its host; build elsewhere, then select the image with the image command.' }
    $repoRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
    Invoke-Docker @('build','-f',(Join-Path $PSScriptRoot 'Dockerfile'),'-t',$imageName,$repoRoot)
    exit 0
}
if ($Action -eq 'image') {
    # Only immutable references: a registry digest (pulled) or the ID of an image already loaded on this host.
    $reference=$TransactionKey
    if ($reference -cmatch '^sha256:[0-9a-f]{64}$') {
        if (!(Test-Image $reference)) { throw "Image $reference is not on this host; load it first (docker save <image> | docker load)." }
    } elseif ($reference -cmatch '^[^\s@]+@sha256:[0-9a-f]{64}$') { Invoke-Docker @('pull',$reference) }
    else { throw 'Usage: ./keeper.ps1 image <image@sha256:digest | sha256:image-id> (mutable tags are refused)' }
    Invoke-Docker @('tag',$reference,$imageName)
    Write-Output "Selected $reference as $imageName"
    exit 0
}
if ($Action -eq 'install') {
    if (!$TransactionKey -or !$VrfKey) { throw 'Usage: ./keeper.ps1 install <transaction.key> <vrf.key>' }
    if (!(Test-Path -LiteralPath $configFile)) { throw "Run init and configure $configName first." }
    if (Select-String -LiteralPath $configFile -Pattern 'VERIFIED_RPC|0x0{40}' -Quiet) { throw 'Replace deployment placeholders first.' }
    if ($instance) { Assert-Image } else { & $PSCommandPath build }
    & $PSCommandPath keys $TransactionKey $VrfKey
    & $PSCommandPath up
    exit 0
}
if ($Action -eq 'keys') {
    if (!$TransactionKey -or !$VrfKey) { throw 'Usage: ./keeper.ps1 keys <transaction.key> <vrf.key>' }
    $keyPaths=@($TransactionKey,$VrfKey) | ForEach-Object {
        $item=Get-Item -LiteralPath $_
        if ($item.PSIsContainer -or $item.FullName.Contains(',')) { throw 'Key must be a file; commas in bind paths are unsupported.' }
        $item.FullName
    }
    Assert-Stopped $keysVolume 'keys' 'provisioning'
    Assert-Image
    Ensure-Volumes
    Invoke-Docker @('run','--rm','--pull','never','--network','none','--user','0:0','--read-only',
        '--mount',"type=volume,source=$keysVolume,target=/run/keeper-keys",
        '--mount',"type=bind,source=$($keyPaths[0]),target=/input/transaction.key,readonly",
        '--mount',"type=bind,source=$($keyPaths[1]),target=/input/vrf.key,readonly",
        '--entrypoint','/usr/local/bin/import-keys.sh',$imageName)
    exit 0
}
if (!(Test-Path -LiteralPath $configFile)) { throw "Run ./keeper.ps1 init and configure $configName first." }
if ($Action -in @('up','restart')) {
    if (Select-String -LiteralPath $configFile -Pattern 'VERIFIED_RPC|0x0{40}' -Quiet) { throw "Replace deployment placeholders in $configName first." }
    Assert-Exclusive
    Ensure-Volumes
    Assert-Image
}
switch ($Action) {
    'up'      { Invoke-Compose @('up','--detach','--no-build','keeper') }
    'restart' { Invoke-Compose @('up','--detach','--no-build','--force-recreate','keeper') }
    'stop'    { Invoke-Compose @('stop','keeper') }
    'down'    { Invoke-Compose @('down') }
    'status'  { Invoke-Compose @('ps','--all') }
    'health'  { Invoke-Compose @('exec','-T','keeper','/usr/local/bin/d20dao-keeper','health') }
    'sweep'   {
        # sweep -SweepAmount <USDC> | -SweepKeep <USDC> | -SweepCancel; no option shows the status.
        if ([bool]$SweepAmount + [bool]$SweepKeep + [bool]$SweepCancel -gt 1) { throw 'Use one of -SweepAmount, -SweepKeep or -SweepCancel' }
        $sweepArgs = if ($SweepAmount) { @('--amount',$SweepAmount) } elseif ($SweepKeep) { @('--keep',$SweepKeep) } elseif ($SweepCancel) { @('--cancel') } else { @('--status') }
        Invoke-Compose (@('exec','-T','keeper','/usr/local/bin/d20dao-keeper','sweep')+$sweepArgs)
    }
    'logs'    { Invoke-Compose @('logs','--follow','--tail','100','keeper') }
    'config'  { Invoke-Compose @('config','--quiet') }
    'migrate' {
        if (!$FromDb -or !$MigrationMode) { throw 'Usage: ./keeper.ps1 migrate -FromDb /var/lib/d20dao/old.sqlite -MigrationMode prepare|apply|resume' }
        if (!$FromDb.StartsWith('/var/lib/d20dao/')) { throw 'Source DB must be inside the persistent state volume.' }
        Assert-Stopped $stateVolume 'state' 'migrating'
        Assert-Stopped $keysVolume 'keys' 'migrating'
        Invoke-Compose @('run','--rm','--no-deps','keeper','migrate','--from',$FromDb,"--$MigrationMode")
    }
}
