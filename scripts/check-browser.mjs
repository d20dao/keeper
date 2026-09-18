import { build } from "esbuild";

// The future verification site must import PUBLIC computation only, not the keeper or test signer.
const result = await build({
  entryPoints: ["src/index.ts"], bundle: true, platform: "browser", format: "esm",
  target: "es2022", write: false, metafile: true, minify: true, logLevel: "warning",
});
const forbidden = Object.keys(result.metafile.inputs).filter(path =>
  /(^|\/)(test|keeper|secrets|\.research)\//.test(path.replaceAll("\\", "/"))
);
if (forbidden.length) throw new Error(`Private/test code entered the public verifier: ${forbidden.join(", ")}`);
const code = result.outputFiles[0].text;
const publicVerifier = await import(`data:text/javascript;base64,${Buffer.from(code).toString("base64")}`);
if (publicVerifier.EPOCH_LENGTH !== 200n || publicVerifier.EVIDENCE_PACKET_BYTES !== 416 || publicVerifier.mapRandomness(`0x${"00".repeat(32)}`, publicVerifier.builtins.d20()).length !== 1)
  throw new Error("Bundled verifier smoke check failed");
console.log(`Public verifier bundles for browsers (${result.outputFiles[0].contents.length} bytes); no keeper/test signer modules included.`);
