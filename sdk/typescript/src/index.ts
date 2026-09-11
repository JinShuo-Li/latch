import { stdin, stdout } from "node:process";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Message = { jsonrpc: "2.0"; id?: Json; method?: string; params?: Json; result?: Json; error?: Json };
type Tool = { name: string; description: string; inputSchema: Json; execute(args: Json): Promise<Json> | Json };

export class LatchExtension {
  private buffer = Buffer.alloc(0);
  private tools = new Map<string, Tool>();
  private commands: string[] = [];
  private nextId = 1;

  tool(tool: Tool): void { this.tools.set(tool.name, tool); }
  command(name: string): void { this.commands.push(name); }

  start(): void {
    stdin.on("data", (chunk: Buffer) => { this.buffer = Buffer.concat([this.buffer, chunk]); this.drain(); });
    stdin.resume();
  }

  private drain(): void {
    for (;;) {
      const end = this.buffer.indexOf("\r\n\r\n");
      if (end < 0) return;
      const header = this.buffer.subarray(0, end).toString("ascii");
      const match = /content-length:\s*(\d+)/i.exec(header);
      if (!match) throw new Error("missing Content-Length");
      const length = Number(match[1]);
      if (this.buffer.length < end + 4 + length) return;
      const body = this.buffer.subarray(end + 4, end + 4 + length);
      this.buffer = this.buffer.subarray(end + 4 + length);
      void this.handle(JSON.parse(body.toString("utf8")) as Message);
    }
  }

  private async handle(message: Message): Promise<void> {
    if (message.method === "initialize") {
      this.respond(message.id, { protocolVersion: "0.1", capabilities: { cooperativePermissions: true } });
    } else if (message.method === "initialized") {
      for (const tool of this.tools.values()) this.request("tool.register", { name: tool.name, description: tool.description, inputSchema: tool.inputSchema });
      for (const name of this.commands) this.request("command.register", { name });
      this.notify("ready", {});
    } else if (message.method === "tool.execute") {
      const params = message.params as { name: string; arguments: Json };
      const tool = this.tools.get(params.name);
      if (!tool) this.error(message.id, -32601, `unknown tool ${params.name}`);
      else { try { this.respond(message.id, await tool.execute(params.arguments)); } catch (error) { this.error(message.id, -32000, String(error)); } }
    } else if (message.method === "shutdown") this.respond(message.id, null);
    else if (message.method === "exit") process.exit(0);
  }

  private send(message: Message): void { const body = Buffer.from(JSON.stringify(message)); stdout.write(`Content-Length: ${body.length}\r\n\r\n`); stdout.write(body); }
  private respond(id: Json | undefined, result: Json): void { this.send({ jsonrpc: "2.0", id, result }); }
  private error(id: Json | undefined, code: number, message: string): void { this.send({ jsonrpc: "2.0", id, error: { code, message } }); }
  private request(method: string, params: Json): void { this.send({ jsonrpc: "2.0", id: this.nextId++, method, params }); }
  private notify(method: string, params: Json): void { this.send({ jsonrpc: "2.0", method, params }); }
}
