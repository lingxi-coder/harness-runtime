function Nt(u){return u.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;")}
function Rs(u){return Nt(u).replaceAll('"',"&quot;").replaceAll("'","&apos;")}
export { Rs };
