import {assistantError, assistantToken} from "@/lib/assistant-server";
import {authenticated} from "@/lib/session-server";

export const runtime = "nodejs";

export async function GET(request: Request, {params}: {params: Promise<{id: string}>}) {
  const session = await authenticated(request);
  if (session !== true) return assistantError(session === false ? 401 : 503, "unavailable", "请先登录");
  const {id} = await params;
  try {
    const response = await fetch("http://127.0.0.1:17863/v1/workbench/knowledge-image/" + encodeURIComponent(id), {
      headers: {authorization: "Bearer " + await assistantToken()},
      cache: "no-store",
      signal: request.signal,
    });
    return new Response(response.body, {status: response.status, headers: {
      "content-type": response.headers.get("content-type") ?? "application/octet-stream",
      "cache-control": "private, max-age=3600",
      "x-content-type-options": "nosniff",
    }});
  } catch {
    return assistantError(503, "unavailable", "图片暂不可用");
  }
}
