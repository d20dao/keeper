// Local-test proxy deployment. Initializer execution is atomic with proxy creation.
export const IMPLEMENTATION_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

export async function deployProxy(ethers:any,name:string,args:readonly unknown[]) {
  const implementation=await ethers.deployContract(name);
  await implementation.waitForDeployment();
  const data=implementation.interface.encodeFunctionData("initialize",args);
  const proxy=await ethers.deployContract("D20Proxy",[await implementation.getAddress(),data]);
  await proxy.waitForDeployment();
  return implementation.attach(await proxy.getAddress());
}

export async function implementationAddress(ethers:any,proxy:any):Promise<string> {
  const stored=await ethers.provider.getStorage(await proxy.getAddress(),IMPLEMENTATION_SLOT);
  return ethers.getAddress("0x"+stored.slice(-40));
}

export async function implementationCodeHash(ethers:any,proxy:any):Promise<string> {
  return ethers.keccak256(await ethers.provider.getCode(await implementationAddress(ethers,proxy)));
}
