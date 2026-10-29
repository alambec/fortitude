# /// script
# requires-python = ">=3.11"
# dependencies = ["pynetdicom>=3.0", "pydicom>=3.0"]
# ///
"""DIMSE interoperability checks of a running Fortitude, with pynetdicom as the SCU and as the
C-MOVE destination.

    uv run tests/interop/dimse.py [--host H] [--port P] [--ae AE] DIR...

The DICOM files under DIR are sent with C-STORE (each on a presentation context of its own
transfer syntax), then queried and moved back. Fortitude must be empty and know the two
destinations started by this script:

    [[dimse.peers]]
    ae_title = "PYNETDICOM"        # accepts every transfer syntax
    host = "127.0.0.1"
    port = 11113
    [[dimse.peers]]
    ae_title = "PYNETDICOM_RAW"    # accepts Explicit/Implicit VR Little Endian only
    host = "127.0.0.1"
    port = 11114
"""

import argparse
import hashlib
import sys
import threading
from collections import defaultdict
from pathlib import Path

from pydicom import dcmread
from pydicom.dataset import Dataset
from pydicom.uid import ExplicitVRLittleEndian, ImplicitVRLittleEndian
from pynetdicom import AE, ALL_TRANSFER_SYNTAXES, StoragePresentationContexts, evt
from pynetdicom.sop_class import (
    PatientRootQueryRetrieveInformationModelFind,
    PatientRootQueryRetrieveInformationModelMove,
    StudyRootQueryRetrieveInformationModelFind,
    StudyRootQueryRetrieveInformationModelMove,
    Verification,
)

UNCOMPRESSED = [ExplicitVRLittleEndian, ImplicitVRLittleEndian]
failures = 0


def check(condition, message):
    global failures
    print(("ok   " if condition else "FAIL ") + message)
    if not condition:
        failures += 1


def associate(args, contexts):
    ae = AE(ae_title="INTEROP")
    ae.acse_timeout = ae.dimse_timeout = ae.network_timeout = 60
    for abstract, syntaxes in contexts:
        ae.add_requested_context(abstract, syntaxes)
    assoc = ae.associate(args.host, args.port, ae_title=args.ae)
    if not assoc.is_established:
        sys.exit("association rejected")
    return assoc


def pixel_hash(ds):
    return hashlib.sha256(ds.PixelData).hexdigest() if "PixelData" in ds else None


class Destination:
    """A storage SCP recording what it receives."""

    def __init__(self, ae_title, port, transfer_syntaxes):
        self.received = {}
        self.syntaxes = {}
        ae = AE(ae_title=ae_title)
        ae.maximum_pdu_size = 0
        for context in StoragePresentationContexts:
            ae.add_supported_context(context.abstract_syntax, transfer_syntaxes)
        self.server = ae.start_server(
            ("127.0.0.1", port), block=False, evt_handlers=[(evt.EVT_C_STORE, self.on_store)]
        )
        self.lock = threading.Lock()

    def on_store(self, event):
        ds = event.dataset
        ds.file_meta = event.file_meta
        with self.lock:
            self.received[ds.SOPInstanceUID] = pixel_hash(ds)
            self.syntaxes[ds.SOPInstanceUID] = event.context.transfer_syntax
        return 0x0000

    def clear(self):
        with self.lock:
            self.received.clear()
            self.syntaxes.clear()


def find(assoc, model, identifier):
    matches, final = [], None
    for status, ds in assoc.send_c_find(identifier, model):
        if status and status.Status in (0xFF00, 0xFF01):
            matches.append(ds)
        else:
            final = status.Status if status else None
    return matches, final


def move(assoc, model, identifier, destination):
    responses = list(assoc.send_c_move(identifier, destination, model))
    status, ds = responses[-1]
    return status, ds, len(responses) - 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=4242)
    parser.add_argument("--ae", default="FORTITUDE")
    parser.add_argument("dirs", nargs="+", type=Path)
    args = parser.parse_args()

    files = sorted(p for d in args.dirs for p in d.rglob("*") if p.is_file() and p.suffix != ".jsonl")
    datasets = [dcmread(p, stop_before_pixels=False) for p in files]
    print(f"{len(files)} files")
    studies = defaultdict(list)
    for ds in datasets:
        studies[ds.StudyInstanceUID].append(ds)

    full = Destination("PYNETDICOM", 11113, ALL_TRANSFER_SYNTAXES)
    raw = Destination("PYNETDICOM_RAW", 11114, UNCOMPRESSED)

    # C-ECHO
    assoc = associate(args, [(Verification, UNCOMPRESSED)])
    status = assoc.send_c_echo()
    check(status.Status == 0x0000, "C-ECHO")
    assoc.release()

    # C-STORE, one context per (SOP class, transfer syntax)
    contexts = sorted({(ds.SOPClassUID, ds.file_meta.TransferSyntaxUID) for ds in datasets})
    check(len(contexts) <= 128, f"{len(contexts)} presentation contexts")
    assoc = associate(args, [(c, [t]) for c, t in contexts])
    statuses = defaultdict(int)
    for path in files:
        status = assoc.send_c_store(path)
        statuses[status.Status if status else None] += 1
    check(statuses == {0x0000: len(files)}, f"C-STORE of {len(files)} files: {dict(statuses)}")
    status = assoc.send_c_store(files[0])
    check(status.Status == 0x0000, "C-STORE of a duplicate succeeds")
    assoc.release()

    q = [
        (StudyRootQueryRetrieveInformationModelFind, UNCOMPRESSED),
        (PatientRootQueryRetrieveInformationModelFind, UNCOMPRESSED),
        (StudyRootQueryRetrieveInformationModelMove, UNCOMPRESSED),
        (PatientRootQueryRetrieveInformationModelMove, UNCOMPRESSED),
    ]
    assoc = associate(args, q)

    # C-FIND at every level
    study_uid, members = max(studies.items(), key=lambda s: len(s[1]))
    first = members[0]
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "STUDY"
    identifier.StudyInstanceUID = study_uid
    identifier.PatientName = ""
    identifier.PatientID = ""
    identifier.StudyDate = ""
    identifier.ModalitiesInStudy = ""
    identifier.NumberOfStudyRelatedSeries = ""
    identifier.NumberOfStudyRelatedInstances = ""
    identifier.RetrieveAETitle = ""
    matches, final = find(assoc, StudyRootQueryRetrieveInformationModelFind, identifier)
    check(final == 0x0000 and len(matches) == 1, f"C-FIND STUDY by UID: {len(matches)} match(es), status {final:#06x}")
    if matches:
        m = matches[0]
        check(str(m.PatientName) == str(first.PatientName), f"PatientName {m.PatientName}")
        check(int(m.NumberOfStudyRelatedInstances) == len(members), f"NumberOfStudyRelatedInstances {m.NumberOfStudyRelatedInstances}")
        series = {ds.SeriesInstanceUID for ds in members}
        check(int(m.NumberOfStudyRelatedSeries) == len(series), f"NumberOfStudyRelatedSeries {m.NumberOfStudyRelatedSeries}")
        check(m.RetrieveAETitle == args.ae, f"RetrieveAETitle {m.RetrieveAETitle}")
        check(m.QueryRetrieveLevel == "STUDY", "QueryRetrieveLevel returned")

    identifier = Dataset()
    identifier.QueryRetrieveLevel = "STUDY"
    identifier.PatientName = str(first.PatientName)[:3].lower() + "*"
    identifier.StudyInstanceUID = ""
    matches, final = find(assoc, StudyRootQueryRetrieveInformationModelFind, identifier)
    expected = {s for s, m in studies.items() if str(m[0].PatientName).upper().startswith(str(first.PatientName)[:3].upper())}
    check({m.StudyInstanceUID for m in matches} == expected, f"C-FIND STUDY by PatientName wildcard: {len(matches)} matches")

    dates = sorted(m[0].StudyDate for m in studies.values())
    low, high = dates[len(dates) // 4], dates[3 * len(dates) // 4]
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "STUDY"
    identifier.StudyDate = f"{low}-{high}"
    identifier.StudyInstanceUID = ""
    matches, final = find(assoc, StudyRootQueryRetrieveInformationModelFind, identifier)
    expected = {s for s, m in studies.items() if low <= m[0].StudyDate <= high}
    check({m.StudyInstanceUID for m in matches} == expected, f"C-FIND STUDY by date range: {len(matches)} matches")

    identifier = Dataset()
    identifier.QueryRetrieveLevel = "PATIENT"
    identifier.PatientID = first.PatientID
    identifier.PatientName = ""
    identifier.NumberOfPatientRelatedStudies = ""
    identifier.NumberOfPatientRelatedInstances = ""
    matches, final = find(assoc, PatientRootQueryRetrieveInformationModelFind, identifier)
    patient_studies = {s for s, m in studies.items() if m[0].PatientID == first.PatientID}
    patient_instances = sum(len(studies[s]) for s in patient_studies)
    check(len(matches) == 1, f"C-FIND PATIENT: {len(matches)} match(es)")
    if matches:
        check(int(matches[0].NumberOfPatientRelatedStudies) == len(patient_studies), "NumberOfPatientRelatedStudies")
        check(int(matches[0].NumberOfPatientRelatedInstances) == patient_instances, "NumberOfPatientRelatedInstances")

    matches, final = find(assoc, StudyRootQueryRetrieveInformationModelFind, identifier)
    check(final == 0xA900 and not matches, f"C-FIND PATIENT level refused on Study Root: {final:#06x}")

    identifier = Dataset()
    identifier.QueryRetrieveLevel = "SERIES"
    identifier.StudyInstanceUID = study_uid
    identifier.SeriesInstanceUID = ""
    identifier.Modality = ""
    identifier.SeriesNumber = ""
    identifier.NumberOfSeriesRelatedInstances = ""
    matches, final = find(assoc, StudyRootQueryRetrieveInformationModelFind, identifier)
    series = defaultdict(int)
    for ds in members:
        series[ds.SeriesInstanceUID] += 1
    check({m.SeriesInstanceUID: int(m.NumberOfSeriesRelatedInstances) for m in matches} == series, f"C-FIND SERIES: {len(matches)} matches")

    series_uid = first.SeriesInstanceUID
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "IMAGE"
    identifier.StudyInstanceUID = study_uid
    identifier.SeriesInstanceUID = series_uid
    identifier.SOPInstanceUID = ""
    identifier.InstanceNumber = ""
    identifier.SOPClassUID = ""
    matches, final = find(assoc, StudyRootQueryRetrieveInformationModelFind, identifier)
    expected = {ds.SOPInstanceUID for ds in members if ds.SeriesInstanceUID == series_uid}
    check({m.SOPInstanceUID for m in matches} == expected, f"C-FIND IMAGE: {len(matches)} matches")

    # C-CANCEL during C-FIND
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "IMAGE"
    identifier.SOPInstanceUID = ""
    responses = assoc.send_c_find(identifier, StudyRootQueryRetrieveInformationModelFind)
    status, _ = next(responses)
    context = next(c for c in assoc.accepted_contexts if c.abstract_syntax == StudyRootQueryRetrieveInformationModelFind)
    assoc.send_c_cancel(1, context.context_id)
    statuses = [s.Status for s, _ in responses if s]
    check(statuses and statuses[-1] == 0xFE00, f"C-CANCEL: {len(statuses)} more responses, final {statuses[-1]:#06x}")

    # C-MOVE to a destination accepting every transfer syntax: files come back unchanged
    expected = {ds.SOPInstanceUID: pixel_hash(ds) for ds in members}
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "STUDY"
    identifier.StudyInstanceUID = study_uid
    status, ds, pending = move(assoc, StudyRootQueryRetrieveInformationModelMove, identifier, "PYNETDICOM")
    check(status.Status == 0x0000, f"C-MOVE STUDY: status {status.Status:#06x}, {pending} pending responses")
    check(status.NumberOfCompletedSuboperations == len(members), f"completed {status.NumberOfCompletedSuboperations}")
    check(full.received == expected, f"{len(full.received)} instances received unchanged")
    stored = {ds.SOPInstanceUID: ds.file_meta.TransferSyntaxUID for ds in members}
    check(full.syntaxes == stored, "sent in their stored transfer syntax")

    # The study with the most transfer syntaxes
    mixed_uid, mixed = max(studies.items(), key=lambda s: len({d.file_meta.TransferSyntaxUID for d in s[1]}))
    full.clear()
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "STUDY"
    identifier.StudyInstanceUID = mixed_uid
    status, _, _ = move(assoc, StudyRootQueryRetrieveInformationModelMove, identifier, "PYNETDICOM")
    stored = {ds.SOPInstanceUID: ds.file_meta.TransferSyntaxUID for ds in mixed}
    check(status.Status == 0x0000 and full.syntaxes == stored, f"C-MOVE of a study in {len(set(stored.values()))} transfer syntaxes, sent as stored")
    check(full.received == {ds.SOPInstanceUID: pixel_hash(ds) for ds in mixed}, "unchanged")

    # C-MOVE of one series and of instances, Patient Root
    full.clear()
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "SERIES"
    identifier.SeriesInstanceUID = series_uid
    status, _, _ = move(assoc, StudyRootQueryRetrieveInformationModelMove, identifier, "PYNETDICOM")
    check(status.Status == 0x0000 and set(full.received) == {u for u in expected if any(d.SOPInstanceUID == u and d.SeriesInstanceUID == series_uid for d in members)}, "C-MOVE SERIES")
    full.clear()
    picked = [ds.SOPInstanceUID for ds in members[:3]]
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "IMAGE"
    identifier.PatientID = first.PatientID
    identifier.StudyInstanceUID = study_uid
    identifier.SeriesInstanceUID = ""
    identifier.SOPInstanceUID = picked
    status, _, _ = move(assoc, PatientRootQueryRetrieveInformationModelMove, identifier, "PYNETDICOM")
    check(status.Status == 0x0000 and sorted(full.received) == sorted(picked), "C-MOVE IMAGE (Patient Root, UID list)")

    full.clear()
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "PATIENT"
    identifier.PatientID = first.PatientID
    status, _, _ = move(assoc, PatientRootQueryRetrieveInformationModelMove, identifier, "PYNETDICOM")
    check(status.Status == 0x0000 and len(full.received) == patient_instances, f"C-MOVE PATIENT: {len(full.received)} instances")

    # C-MOVE to a destination accepting only uncompressed syntaxes: compressed files are
    # transcoded (or fail when they cannot be decoded)
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "STUDY"
    identifier.StudyInstanceUID = mixed_uid
    status, ds, _ = move(assoc, StudyRootQueryRetrieveInformationModelMove, identifier, "PYNETDICOM_RAW")
    completed, failed = status.NumberOfCompletedSuboperations, status.NumberOfFailedSuboperations
    check(completed + failed == len(mixed) and set(raw.syntaxes.values()) <= set(UNCOMPRESSED), f"C-MOVE to an uncompressed-only destination: {completed} completed, {failed} failed, status {status.Status:#06x}")
    if failed:
        check(status.Status == 0xB000 and ds is not None and len(ds.FailedSOPInstanceUIDList) == failed, "failures listed")

    # Errors
    status, _, _ = move(assoc, StudyRootQueryRetrieveInformationModelMove, identifier, "NOWHERE")
    check(status.Status == 0xA801, f"C-MOVE to an unknown destination: {status.Status:#06x}")
    identifier = Dataset()
    identifier.QueryRetrieveLevel = "SERIES"
    identifier.StudyInstanceUID = study_uid
    status, _, _ = move(assoc, StudyRootQueryRetrieveInformationModelMove, identifier, "PYNETDICOM")
    check(status.Status == 0xA900, f"C-MOVE without the unique key of its level: {status.Status:#06x}")
    assoc.release()

    full.server.shutdown()
    raw.server.shutdown()
    print(f"{failures} failure(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
