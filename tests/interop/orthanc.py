"""DIMSE interoperability checks between Orthanc and a running Fortitude: Orthanc as C-ECHO,
C-STORE, C-FIND and C-MOVE SCU, and as the C-MOVE destination.

    docker compose -f tests/interop/orthanc-compose.yml up -d
    python3 tests/interop/orthanc.py [--orthanc URL] [--fortitude URL] DIR...

The DICOM files under DIR are uploaded to Orthanc, pushed to Fortitude, queried from
Orthanc, deleted from Orthanc and moved back from Fortitude. Fortitude must be empty, listen
for DIMSE on 127.0.0.1:4250 as FORTITUDE, and know Orthanc:

    [[dimse.peers]]
    ae_title = "ORTHANC"
    host = "127.0.0.1"
    port = 4247
"""

import argparse
import json
import sys
import urllib.request
from pathlib import Path

failures = 0


def check(condition, message):
    global failures
    print(("ok   " if condition else "FAIL ") + message)
    if not condition:
        failures += 1


def call(base, method, path, body=None, raw=False):
    data = body if raw else (json.dumps(body).encode() if body is not None else None)
    request = urllib.request.Request(base + path, data=data, method=method)
    if data is not None and not raw:
        request.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(request, timeout=600) as response:
        payload = response.read()
    return json.loads(payload) if payload else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--orthanc", default="http://127.0.0.1:8047")
    parser.add_argument("--fortitude", default="http://127.0.0.1:8046")
    parser.add_argument("dirs", nargs="+", type=Path)
    args = parser.parse_args()
    orthanc = lambda method, path, body=None, raw=False: call(args.orthanc, method, path, body, raw)

    for study in orthanc("GET", "/studies"):
        orthanc("DELETE", f"/studies/{study}")
    files = sorted(p for d in args.dirs for p in d.rglob("*.dcm"))
    for path in files:
        orthanc("POST", "/instances", path.read_bytes(), raw=True)
    studies = orthanc("GET", "/studies?expand")
    instances = {
        i["MainDicomTags"]["SOPInstanceUID"]: i["ID"] for i in orthanc("GET", "/instances?expand")
    }
    syntaxes = {
        uid: orthanc("GET", f"/instances/{oid}/metadata?expand")["TransferSyntax"]
        for uid, oid in instances.items()
    }
    print(f"{len(files)} files, {len(studies)} studies in Orthanc")

    echo = orthanc("POST", "/modalities/fortitude/echo", {})
    check(echo == {} or echo is None, "C-ECHO from Orthanc")

    # Orthanc proposes two uncompressed contexts for each of its 64 usual storage classes,
    # which leaves room for an instance's own transfer syntax only when the job is small:
    # compressed instances are sent one per job, so that Orthanc does not decompress them.
    uncompressed = {"1.2.840.10008.1.2", "1.2.840.10008.1.2.1"}
    jobs = [[oid for uid, oid in instances.items() if syntaxes[uid] in uncompressed]]
    jobs += [[oid] for uid, oid in instances.items() if syntaxes[uid] not in uncompressed]
    sent = failed = 0
    for resources in jobs:
        job = orthanc("POST", "/modalities/fortitude/store", {"Resources": resources, "Synchronous": True})
        sent += job["InstancesCount"]
        failed += job["FailedInstancesCount"]
    check(failed == 0 and sent == len(instances), f"C-STORE from Orthanc: {sent} instances, {failed} failed")
    statistics = call(args.fortitude, "GET", "/admin/statistics")
    check(statistics["instances"] == len(instances), f"Fortitude holds {statistics['instances']} instances")

    counts = {s["ID"]: len(orthanc("GET", f"/studies/{s['ID']}/instances")) for s in studies}
    study = max(studies, key=lambda s: counts[s["ID"]])
    tags = study["MainDicomTags"]
    patient = study["PatientMainDicomTags"]
    query = orthanc("POST", "/modalities/fortitude/query", {
        "Level": "Study",
        "Query": {"PatientID": patient["PatientID"], "StudyDate": "", "PatientName": "", "NumberOfStudyRelatedInstances": ""},
    })
    answers = orthanc("GET", f"/queries/{query['ID']}/answers?expand&simplify")
    check(len(answers) >= 1 and all(a["PatientID"] == patient["PatientID"] for a in answers), f"C-FIND STUDY from Orthanc: {len(answers)} answer(s)")
    mine = [a for a in answers if a["StudyInstanceUID"] == tags["StudyInstanceUID"]]
    check(len(mine) == 1 and int(mine[0]["NumberOfStudyRelatedInstances"]) == counts[study["ID"]], "study and its instance count found")

    series_query = orthanc("POST", "/modalities/fortitude/query", {
        "Level": "Series",
        "Query": {"StudyInstanceUID": tags["StudyInstanceUID"], "SeriesInstanceUID": "", "Modality": ""},
    })
    series_answers = orthanc("GET", f"/queries/{series_query['ID']}/answers?expand&simplify")
    check(len(series_answers) == len(study["Series"]), f"C-FIND SERIES from Orthanc: {len(series_answers)} answer(s)")
    instance_query = orthanc("POST", "/modalities/fortitude/query", {
        "Level": "Instance",
        "Query": {"StudyInstanceUID": tags["StudyInstanceUID"], "SeriesInstanceUID": series_answers[0]["SeriesInstanceUID"], "SOPInstanceUID": ""},
    })
    instance_answers = orthanc("GET", f"/queries/{instance_query['ID']}/answers?expand&simplify")
    series = orthanc("GET", "/series?expand")
    expected = next(len(s["Instances"]) for s in series if s["MainDicomTags"]["SeriesInstanceUID"] == series_answers[0]["SeriesInstanceUID"])
    check(len(instance_answers) == expected, f"C-FIND IMAGE from Orthanc: {len(instance_answers)} answer(s)")

    # Delete everything from Orthanc, then C-MOVE it back from Fortitude.
    for s in studies:
        orthanc("DELETE", f"/studies/{s['ID']}")
    for s in studies:
        query = orthanc("POST", "/modalities/fortitude/query", {
            "Level": "Study", "Query": {"StudyInstanceUID": s["MainDicomTags"]["StudyInstanceUID"]},
        })
        orthanc("POST", f"/queries/{query['ID']}/retrieve", {"TargetAet": "ORTHANC", "Synchronous": True})
    back = {i["MainDicomTags"]["SOPInstanceUID"]: i["ID"] for i in orthanc("GET", "/instances?expand")}
    check(set(back) == set(instances), f"C-MOVE to Orthanc: {len(back)} of {len(instances)} instances back")
    back_syntaxes = {
        uid: orthanc("GET", f"/instances/{oid}/metadata?expand")["TransferSyntax"]
        for uid, oid in back.items()
    }
    check(back_syntaxes == {u: syntaxes[u] for u in back}, "in their original transfer syntax")
    print(f"{failures} failure(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
