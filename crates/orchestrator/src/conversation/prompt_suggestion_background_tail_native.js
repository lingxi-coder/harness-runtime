const backgroundTail = (() => {
const {mRo,fRo,N0s,pRo,B0s,E1e,xzn} = redactor;
const {wZr} = classifier;
var ke=8000,ge=8*E1e;
function Pe(e){let n=fRo(e),r=ge+3*(ke+2*E1e),u=n.lastIndexOf(`
`,n.trimEnd().length-1)+1,o=n.length;for(;;r*=2){let l=r>=n.length?0:n.lastIndexOf(`
`,n.length-r-1)+1;if(l=ut(n,l),l<=0)return Le(mRo(n),"").redacted;if(l>=o||u-l<ge){o=Math.min(o,l);continue}o=l;let s=n.slice(0,l),h=mRo(n.slice(l)),{redacted:p,dropped:C}=Le(h,s.trim()===""?"":wZr(s.trimStart())||`\u2026
`),E=0;for(let A=h.indexOf("[REDACTED]");A!==-1&&A<C;A=h.indexOf("[REDACTED]",A+10))E++;if(C-10*E>=ge)return p}}
function Re(e,n){while(n>0&&pRo.test(e[n-1]))n--;return n}
function ut(e,n){if(n<=0)return n;let r=Re(e,n),u=!1,o=n;while(o<e.length&&pRo.test(e[o]))o++;if(e[o]==="="||e[o]===":")u=!0;for(let l=0;l<8;l++){let s=r;for(let C=0;s>0&&C<8;s--)if(!N0s.test(e[s-1]))C++;let h=e.slice(s,r),p=B0s.exec(h);if(!p)break;u=!0,r=Re(e,r-p[0].length)}return u?e.lastIndexOf(`
`,r-1)+1:n}
function Le(e,n){let r=ke+2*E1e,u=e.length;for(;;r*=2){let o=r>=e.length?0:e.lastIndexOf(`
`,e.length-r-1)+1;if(o<=0)return{redacted:n+xzn(e),dropped:0};if(o>=u)continue;u=o;let l=xzn(e.slice(o)),s=l.trimEnd().length,h=l.lastIndexOf(`
`,s-1)+1;if(s-ke>=E1e&&h>=E1e){let p=n+e.slice(0,o);return{redacted:p.trim()===""?l:(wZr(p.trimStart())||`\u2026
`)+l,dropped:o}}}}
return Pe;
})();
