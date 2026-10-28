"""Compare series metadata of two DICOMweb servers loaded with the same dataset.

    python3 bench/diff_metadata.py http://localhost:8043/dicom-web http://localhost:8045/dicom-web \
        bench/datasets/mixed/manifest.jsonl

Compares one series per (modality, transfer syntax) of the first copy of the dataset and prints
the attributes that differ, ignoring representation choices: bulk data encoding, DS/IS as strings
or numbers, Specific Character Set, and private attributes (whose VR depends on the server's
private dictionary).
"""
import json, sys, urllib.request, collections

A, B, manifest = sys.argv[1], sys.argv[2], sys.argv[3]

def get(base, path):
    req = urllib.request.Request(base + path, headers={"Accept": "application/dicom+json"})
    return json.load(urllib.request.urlopen(req))

def norm(v):
    # Bulk data representation is a server choice; compare presence only.
    if isinstance(v, dict):
        if "BulkDataURI" in v or "InlineBinary" in v:
            return {"vr": v.get("vr"), "binary": True}
        if v.get("vr") in ("DS", "IS") and "Value" in v:
            # Orthanc returns DS/IS as strings, PS3.18 F.2.3 as numbers.
            return {"vr": v["vr"], "Value": [None if x is None else round(float(x), 6) for x in v["Value"]]}
        return {k: norm(x) for k, x in v.items()}
    if isinstance(v, list):
        return [norm(x) for x in v]
    if isinstance(v, float) and v.is_integer():
        return int(v)
    return v

entries = [json.loads(l) for l in open(manifest)]
seen, picked = set(), []
for e in entries:
    if e["path"].startswith("00000/") and e["series_uid"] not in seen:
        seen.add(e["series_uid"]); picked.append(e)

diffs = collections.Counter()
examples = {}
for e in picked:
    path = f"/studies/{e['study_uid']}/series/{e['series_uid']}/metadata"
    a = {i["00080018"]["Value"][0]: i for i in get(A, path)}
    b = {i["00080018"]["Value"][0]: i for i in get(B, path)}
    assert a.keys() == b.keys(), path
    for sop in a:
        ia, ib = a[sop], b[sop]
        for tag in sorted((set(ia) | set(ib)) - {"00080005"}):
            if int(tag[3], 16) % 2 == 1:
                continue  # private: VR depends on the private dictionary
            va, vb = norm(ia.get(tag)), norm(ib.get(tag))
            if va != vb:
                key = (tag, "missing-in-A" if va is None else "missing-in-B" if vb is None else "value")
                diffs[key] += 1
                examples.setdefault(key, (e["modality"], va, vb))
print(f"compared {len(picked)} series")
for key, n in diffs.most_common(40):
    m, va, vb = examples[key]
    print(n, key, m, "A=", json.dumps(va)[:150], "B=", json.dumps(vb)[:150])
