import os,re,sys,json
# usage: oracle_prose_delta.py OLD_BINARY NEW_BINARY OUT.json
#   OLD_BINARY: e.g. an `npm pack @anthropic-ai/claude-code-darwin-arm64@X` extract
#   NEW_BINARY: e.g. ~/.local/share/claude/versions/2.1.263
OLD, NEW = sys.argv[1], sys.argv[2]

# Pull ASCII-ish prose strings from a binary's string table + source chunks.
PROSE=re.compile(rb"[\x20-\x7e]{24,400}")
def prose(path):
    data=open(path,'rb').read()
    out=set()
    for m in PROSE.finditer(data):
        s=m.group(0)
        # keep sentence-ish strings (a space and a lowercase letter)
        if b' ' not in s: continue
        if not re.search(rb"[a-z]{3}", s): continue
        out.add(s)
    return out

old=prose(OLD); new=prose(NEW)
print("2.1.232 prose:",len(old))
print("2.1.263 prose:",len(new))
added = new - old
print("added in 2.1.263:",len(added))
json.dump(sorted(s.decode('utf-8','replace') for s in added), open(sys.argv[3],"w"))
