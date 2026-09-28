import os,sys,json,re
B=os.path.expanduser("~/.local/share/claude/versions/2.1.263")
data=open(B,'rb').read()
rows=json.load(open(sys.argv[1]))

def variants(v):
    out=set([v.encode()])
    for nl in (True,False):
        for esc in ("u","x","none"):
            for q in (None,'"',"'","`"):
                b=[]
                for ch in v:
                    o=ord(ch)
                    if o>127:
                        if esc=="u": b.append("\\u%04x"%o)
                        elif esc=="x" and o<256: b.append("\\x%02X"%o)
                        else: b.append(ch)
                    elif ch=="\n" and nl: b.append("\\n")
                    elif q and ch==q: b.append("\\"+ch)
                    else: b.append(ch)
                out.add("".join(b).encode())
    return out

PLACEHOLDER=re.compile(r'\{[^{}]*\}')
def segments(v):
    v=v.replace("{{","\x01").replace("}}","\x02")
    parts=PLACEHOLDER.split(v)
    parts=[p.replace("\x01","{").replace("\x02","}") for p in parts]
    # collapse runs of whitespace/newlines: Rust source wraps long literals
    return [p for p in parts if len(p.strip())>=12]

cache={}
def found(seg):
    if seg in cache: return cache[seg]
    r=any(data.find(b)>=0 for b in variants(seg))
    cache[seg]=r
    return r

hits=[];miss=[];nosegs=[]
for p,v in rows:
    segs=segments(v)
    if not segs:
        nosegs.append((p,v)); continue
    bad=[s for s in segs if not found(s)]
    if bad: miss.append((p,v,bad))
    else: hits.append((p,v))
print(f"literals {len(rows)}  segment-verified HIT {len(hits)}  MISS {len(miss)}  no-usable-segment {len(nosegs)}")
json.dump(miss,open(sys.argv[2],"w"))
from collections import Counter
for p,n in Counter(p for p,_,_ in miss).most_common(50): print(f"{n:5d}  {p}")
