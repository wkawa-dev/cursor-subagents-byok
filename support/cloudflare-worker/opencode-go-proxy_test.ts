import {
  deterministicSessionId,
  ensureDeepSeekReasoning,
} from "./opencode-go-proxy.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, got ${left}`);
}

function toolRequest(model: string, input: unknown[]): Record<string, unknown> {
  return {
    model,
    input,
    tools: [{
      type: "function",
      name: "get_weather",
      parameters: { type: "object", properties: {} },
    }],
  };
}

Deno.test("session id is stable per key and model", async () => {
  const first = await deterministicSessionId("Bearer sk_test", "deepseek-v4.1-flash");
  const second = await deterministicSessionId("Bearer sk_test", "deepseek-v4.1-flash");
  const otherModel = await deterministicSessionId("Bearer sk_test", "deepseek-v4-pro");
  const otherKey = await deterministicSessionId("Bearer sk_other", "deepseek-v4.1-flash");

  assertEquals(first, second);
  assert(first !== otherModel, "different models must not share a session");
  assert(first !== otherKey, "different keys must not share a session");
  assert(/^ses_[0-9a-f]{24}$/.test(first), `unexpected session shape: ${first}`);
});

Deno.test("deepseek tool turns receive a reasoning item", () => {
  const body = toolRequest("deepseek-v4.1-flash", [
    { type: "message", role: "user", content: [{ type: "input_text", text: "weather" }] },
    { type: "function_call", call_id: "call_1", name: "get_weather", arguments: "{}" },
    { type: "function_call_output", call_id: "call_1", output: "18" },
    { type: "function_call", call_id: "call_2", name: "get_weather", arguments: "{}" },
    { type: "function_call_output", call_id: "call_2", output: "17" },
  ]);

  assert(ensureDeepSeekReasoning(body), "missing reasoning must be patched");
  const input = body.input as Array<Record<string, unknown>>;
  assertEquals(input.map((item) => item.type), [
    "message",
    "reasoning",
    "function_call",
    "function_call_output",
    "reasoning",
    "function_call",
    "function_call_output",
  ]);
  assertEquals(body.input, input);
});

Deno.test("deepseek patch is idempotent and preserves existing reasoning", () => {
  const body = toolRequest("deepseek-v4.1-flash", [
    { type: "reasoning", summary: [], content: [] },
    { type: "function_call", call_id: "call_1", name: "get_weather", arguments: "{}" },
  ]);

  assert(!ensureDeepSeekReasoning(body), "existing reasoning must not be duplicated");
  assert(ensureDeepSeekReasoning(body) === false, "second pass must be a no-op");
  assertEquals((body.input as unknown[]).length, 2);
});

Deno.test("deepseek chat turns receive reasoning_content", () => {
  const body: Record<string, unknown> = {
    model: "deepseek-v4.1-flash",
    tools: [{ type: "function", function: { name: "get_weather" } }],
    messages: [
      { role: "user", content: "weather" },
      { role: "assistant", content: null, tool_calls: [{ id: "call_1" }] },
      { role: "tool", tool_call_id: "call_1", content: "18" },
      { role: "assistant", content: "18 C" },
    ],
  };

  assert(ensureDeepSeekReasoning(body), "chat history must be patched");
  assertEquals(
    (body.messages as Array<Record<string, unknown>>).map((message) => message.reasoning_content),
    [undefined, "", undefined, ""],
  );
  assert(!ensureDeepSeekReasoning(body), "second chat pass must be a no-op");
});

Deno.test("other models and tool-less requests are untouched", () => {
  const otherModel = toolRequest("glm-5.3", [
    { type: "function_call", call_id: "call_1", name: "get_weather", arguments: "{}" },
  ]);
  assert(!ensureDeepSeekReasoning(otherModel), "non-deepseek model must be untouched");

  const noTools: Record<string, unknown> = {
    model: "deepseek-v4.1-flash",
    input: [
      { type: "function_call", call_id: "call_1", name: "get_weather", arguments: "{}" },
    ],
  };
  assert(!ensureDeepSeekReasoning(noTools), "tool-less request must be untouched");
  assertEquals((noTools.input as unknown[]).length, 1);
});
