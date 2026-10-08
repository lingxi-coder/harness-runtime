export function register(on) {
  on("session.append", async ($,e,next) => {
    if (e.uuid === "rewrite") {
      return next({...e,message:{...e.message,content:e.message.content.map(b=>({...b,text:"prefix:"+b.text}))}});
    }
    if (e.uuid === "00000000-0000-4000-8000-000000000015") {
      return next({...e,message:{...e.message,content:e.message.content.map(b=>({...b,text:"system-prefix:"+b.text}))}});
    }
    return next(e);
  });
}
