// Preloaded with `node --import` into a script under test, to prove it stays off the network: a socket, a DNS lookup or a fetch ends the
// process with status 97 and a line on stderr, so a refusal that came after network access is told from one that came before it.
import net from "node:net";
import dns from "node:dns";

const stop=what=>{process.stderr.write(`NETWORK ACCESS: ${what}\n`);process.exit(97);};
net.Socket.prototype.connect=function connect(){stop("socket");};
dns.lookup=function lookup(){stop("dns lookup");};
dns.promises.lookup=function lookup(){stop("dns lookup");};
globalThis.fetch=function fetch(){stop("fetch");};
