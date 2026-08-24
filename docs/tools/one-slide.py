import copy, sys
from pptx import Presentation

src, idx, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
prs = Presentation(src)
lst = prs.slides._sldIdLst
ids = list(lst)
keep = ids[idx]
for sid in ids:
    if sid is not keep:
        rid = sid.get('{http://schemas.openxmlformats.org/officeDocument/2006/relationships}id')
        prs.part.drop_rel(rid)
        lst.remove(sid)
prs.save(out)
