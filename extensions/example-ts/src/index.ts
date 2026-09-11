import { LatchExtension, type Json } from "@latch-agent/extension-sdk";

const extension = new LatchExtension();
extension.tool({
  name: "example.echo",
  description: "Return the supplied value to prove cross-language tool execution.",
  inputSchema: { type: "object", properties: { value: {} }, required: ["value"] },
  execute(args: Json): Json { return { echoed: (args as { value: Json }).value }; },
});
extension.command("example-about");
extension.start();
