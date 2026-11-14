import { readRequestBody } from '@/lib/request-body';
import {authenticated} from "@/lib/session-server";
import {allowedOrigin,assistantError,assistantToken} from "@/lib/assistant-server";
export const runtime="nodejs";
export async function POST(request:Request,{params}:{params:Promise<{operation:string}>}){
 const session=await authenticated(request);if(session!==true)return assistantError(session===false?401:503,"unavailable",session===false?"请先登录":"服务正在恢复");
 if(!allowedOrigin(request))return assistantError(403,"wrong_origin","请从工作台操作");const {operation}=await params;
 if(!["memories","memory-save","memory-delete","skills","skill-import","skill-update","skill-edit","knowledge","knowledge-read","knowledge-save","knowledge-link","knowledge-related","knowledge-delete","knowledge-image-upload","files","events","upload-file"].includes(operation))return assistantError(404,"not_found","操作不存在");
 const upload=operation==="upload-file",limit=upload?12*1024*1024:operation==="knowledge-image-upload"?6*1024*1024+16:2*1024*1024;const type=request.headers.get("content-type")||"";
 if(!upload&&type!=="application/x-protobuf")return assistantError(415,"unsupported_media_type","请求格式错误");
 try{const body=await readRequestBody(request,limit);if(body instanceof Response)return body;const response=await fetch("http://127.0.0.1:17863/v1/workbench/"+operation,{method:"POST",headers:{authorization:"Bearer "+await assistantToken(),"content-type":type},body,cache:"no-store",signal:AbortSignal.timeout(150000)});return new Response(await response.arrayBuffer(),{status:response.status,headers:{"content-type":"application/x-protobuf","cache-control":"no-store"}});}catch{return assistantError(503,"unavailable","工作服务暂不可用，请重试");}
}
