// Preloaded with `node --import` into scripts/deploy-robinhood.ts by its tests, which run from a working tree under review: it sets the global
// that lets reviewedSource (scripts/lib/robinhood-deploy.ts) skip its check of a clean, pushed commit. The deployment script honors it only
// against a local node (--rpc-url on 127.0.0.1, localhost or [::1]), never against a chain's endpoint, and records in the manifest that the
// source was not checked. No option of the command line sets it.
globalThis[Symbol.for("d20dao.tests.unreviewedSource")]=true;
