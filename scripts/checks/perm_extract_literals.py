import os,re,sys,json
ROOT="crates/permission/src"
# collect string literals outside #[cfg(test)] regions
def strip_tests(src):
    out=[];i=0;n=len(src)
    while True:
        m=re.search(r'#\[cfg\(test\)\]', src[i:])
        if not m: out.append(src[i:]); break
        s=i+m.start(); out.append(src[i:s])
        # find the module body braces after the attribute
        j=src.find('{', s)
        if j<0: break
        d=0;k=j
        while k<n:
            if src[k]=='{': d+=1
            elif src[k]=='}':
                d-=1
                if d==0: break
            k+=1
        i=k+1
    return "".join(out)

LIT=re.compile(r'"((?:[^"\\]|\\.)*)"', re.S)
def unrust(s):
    out=[];i=0
    while i<len(s):
        if s[i]=='\\' and i+1<len(s):
            c=s[i+1]
            if c=='u' and i+2<len(s) and s[i+2]=='{':
                j=s.index('}', i+2)
                out.append(chr(int(s[i+3:j],16))); i=j+1; continue
            if c=='\n':
                # Rust line-continuation: the backslash, the newline and ALL
                # leading whitespace on the next line are removed.
                i+=2
                while i<len(s) and s[i] in ' \t': i+=1
                continue
            if c=='x' and i+3<len(s):
                out.append(chr(int(s[i+2:i+4],16))); i+=4; continue
            out.append({'n':'\n','t':'\t','r':'\r','\\':'\\','"':'"',"'":"'",'0':'\0'}.get(c,c)); i+=2
        else: out.append(s[i]); i+=1
    return "".join(out)

rows=[]
for dirpath,_,files in os.walk(ROOT):
    for fn in sorted(files):
        if not fn.endswith(".rs"): continue
        if fn.endswith("_test.rs"): continue
        p=os.path.join(dirpath,fn)
        src=open(p,encoding="utf-8").read()
        body=strip_tests(src)
        # drop line comments & doc comments (parity notes quote oracle text; not shipped strings)
        body=re.sub(r'^\s*//.*$','',body,flags=re.M)
        for m in LIT.finditer(body):
            v=unrust(m.group(1))
            if len(v)<20: continue
            if ' ' not in v: continue
            rows.append((p,v))
print(len(rows),"candidate literals", file=sys.stderr)
json.dump(rows, open(sys.argv[1],"w"))
