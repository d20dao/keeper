import {execFile} from "node:child_process";
import {promisify} from "node:util";
const execute=promisify(execFile);

export class RemotePilot {
  readonly host:string;
  readonly directory:string;
  constructor(host:string,directory:string){
    if(!/^[a-zA-Z0-9_.@-]+$/.test(host)||host.startsWith("-")||!/^\/[a-zA-Z0-9_./-]+$/.test(directory)||directory.includes(".."))throw new Error("Invalid SSH pilot destination");
    this.host=host;this.directory=directory;
  }
  private async run(command:string){
    const result=await execute("ssh",["-o","BatchMode=yes","-o","ConnectTimeout=10",this.host,command],{windowsHide:true,timeout:45000,maxBuffer:262144});
    return result.stdout.trim();
  }
  async start(){await this.run(`sudo -n sh ${this.directory}/deploy/docker/keeper.sh up`);}
  async stop(){await this.run(`sudo -n sh ${this.directory}/deploy/docker/keeper.sh stop`);}
  async observe<T>(action:"running"|"ready"|"attempts",epoch?:bigint):Promise<T>{
    return JSON.parse(await this.run(`sudo -n python3 ${this.directory}/scripts/keeper-pilot.py ${action}${epoch===undefined?"":` ${epoch}`}`)) as T;
  }
}
