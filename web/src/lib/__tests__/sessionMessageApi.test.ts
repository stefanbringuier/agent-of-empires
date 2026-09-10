import { afterEach, describe, expect, it, vi } from "vitest";
import { sendSessionMessage } from "../api";

afterEach(() => vi.unstubAllGlobals());

describe("sendSessionMessage", () => {
  it("preserves exact content and distinguishes host outcomes without retries", async () => {
    const cases = [
      { response: new Response(JSON.stringify({ status: "queued" })), expected: "queued" },
      { response: new Response("Recipient stopped", { status: 409 }), expected: "error" },
      { response: new Response("Lost worker response", { status: 503 }), expected: "unknown" },
      { response: new Response("{}"), expected: "unknown" },
      { response: null, expected: "unknown" },
    ];
    for (const { response, expected } of cases) {
      const fetch = vi.fn();
      if (response) fetch.mockResolvedValue(response);
      else fetch.mockRejectedValue(new Error("offline"));
      vi.stubGlobal("fetch", fetch);
      expect((await sendSessionMessage("source", "stable/id", " exact\n")).status).toBe(expected);
      expect(fetch).toHaveBeenCalledExactlyOnceWith("/api/sessions/stable%2Fid/message", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ source_session_id: "source", text: " exact\n" }),
      });
    }
  });
});
