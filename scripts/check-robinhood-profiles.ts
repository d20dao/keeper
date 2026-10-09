// The Robinhood legacy gate (npm run robinhood:profiles, in npm run check): every Robinhood profile, the round coordinator's deployment and
// administration scripts and the Robinhood deployment manifests (scripts/lib/robinhood-profiles.ts) are free of the epoch design's words,
// Arc's name, Arc's chain ids and Arc's owners. Nothing is allowed through.
import {legacyFindings,robinhoodSurfaces} from "./lib/robinhood-profiles.ts";

const {surfaces,markers}=await robinhoodSurfaces();
const findings=surfaces.flatMap(surface=>legacyFindings(surface,markers));
if(findings.length){
  console.error(`The Robinhood profiles say what they must not:\n${findings.map(line=>`  ${line}`).join("\n")}`);
  process.exit(1);
}
console.log(`The ${surfaces.length} Robinhood profiles, scripts and manifests name no registry, epoch, recipe, AirnodeHub, nudge, USDC, committer or confirmations, and none of Arc's name, chain ids (${markers.chainIds.join(", ")}) or ${markers.owners.length} owners.`);
