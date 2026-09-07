"""Independent file-progress alerts. Never opens a device or changes recordings."""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import secrets
import sys
import time
import urllib.request
from dataclasses import dataclass


@dataclass(frozen=True)
class Notice:
    key: str
    text: str
    priority: int = 4


class ProgressWatch:
    def __init__(self, now: float, baseline: dict, timeout: float = 60):
        self.previous = baseline
        self.last_growth = {".raw": now, ".pdq": now}
        self.timeout = timeout
        self.active = False
        self.alerted = set()
        self.seen_sidecars = {p: signature for p, signature in baseline.items()
                              if p.endswith(".a2.json")}

    def tick(self, now: float, snapshot: dict) -> list[Notice]:
        notices = []
        for path, signature in snapshot.items():
            ext = Path(path).suffix.lower()
            if ext not in self.last_growth:
                continue
            old = self.previous.get(path)
            # Timestamps alone are not evidence of captured data.
            if signature[0] > (old[0] if old else 0):
                if not self.active:
                    self.last_growth = {kind: now for kind in self.last_growth}
                    self.active = True
                self.last_growth[ext] = now
                if ext in self.alerted:
                    self.alerted.remove(ext)
                    notices.append(Notice("resumed:" + ext, f"{ext[1:].upper()}: Dateifortschritt wieder sichtbar.", 3))
        for ext, at in self.last_growth.items():
            if now - at >= self.timeout and ext not in self.alerted:
                self.alerted.add(ext)
                notices.append(Notice("stalled:" + ext,
                    f"Seit {int(self.timeout)} s kein neuer {ext[1:].upper()}-Dateifortschritt. "
                    "Bitte Augur pruefen: Start ausstehend, Bedienpause, Ende oder Stoerung. "
                    "Dies ist keine automatische Fehlerdiagnose."))
        self.previous = snapshot
        return notices

    def read_changed_sidecars(self, snapshot: dict) -> list[Notice]:
        notices = []
        for name, signature in snapshot.items():
            if not name.endswith(".a2.json") or self.seen_sidecars.get(name) == signature:
                continue
            try:
                # A partial JSON write is retried on the next poll.
                if signature[0] > 2_000_000:
                    continue
                data = json.loads(Path(name).read_text(encoding="utf-8"))
                if not isinstance(data, dict):
                    continue
                evidence = data.get("evidence", {})
                if not isinstance(evidence, dict) or data.get("experiment") != "A2":
                    continue
                row = data.get("protocol_row", "?")
                failed = bool(evidence.get("failure") or data.get("cleanup_failures"))
                previous = self.seen_sidecars.get(name)
                self.seen_sidecars[name] = signature
                if failed:
                    # Keep paths, sample names and raw error payloads on the lab PC.
                    notices.append(Notice("failed:" + name, f"A2 Punkt {row}: Aufnahmefehler oder Abschalten nicht bestaetigt. Details stehen lokal in Augur und der A2-Datei."))
                elif previous is None and evidence.get("warnings"):
                    notices.append(Notice("review:" + name, f"A2 Punkt {row}: Dateien gespeichert; Zeitreferenz/Diagnostik erfordert Nachpruefung.", 3))
            except (OSError, ValueError, TypeError):
                continue
        return notices


def snapshot_folders(folders: list[Path]) -> dict:
    result = {}
    def traversal_error(error):
        raise error
    for folder in folders:
        if not folder.is_dir():
            raise OSError("Ein ausgewaehlter Aufnahmeordner ist nicht mehr erreichbar.")
        for root, _, files in os.walk(folder, onerror=traversal_error, followlinks=False):
            for name in files:
                if not (name.lower().endswith((".raw", ".pdq")) or name.endswith(".a2.json")):
                    continue
                path = Path(root) / name
                try:
                    stat = path.stat()
                except FileNotFoundError:
                    continue  # a recorder can rename a finalized file during enumeration
                result[str(path)] = (stat.st_size, stat.st_mtime_ns)
    return result


def publish(topic: str, notice: Notice, opener=urllib.request.urlopen):
    if not re.fullmatch(r"[A-Za-z0-9_-]{16,100}", topic):
        raise ValueError("Ungueltiges ntfy-Thema; mindestens 16 Zeichen, nur Buchstaben, Zahlen, _ und -.")
    request = urllib.request.Request("https://ntfy.sh/" + topic,
        data=notice.text.encode("utf-8"), method="POST",
        headers={"Title": "Augur Labor", "Priority": str(notice.priority),
                 "Content-Type": "text/plain; charset=utf-8"})
    with opener(request, timeout=8) as response:
        if not 200 <= response.status < 300:
            raise OSError("Der Push-Dienst hat die Nachricht nicht angenommen.")


class PendingNotices:
    def __init__(self):
        self.items = {}

    def add(self, notice):
        if notice.key.startswith("resumed:"):
            self.items.pop("stalled:" + notice.key.split(":", 1)[1], None)
        if len(self.items) >= 100 and notice.key not in self.items:
            self.items = {"overflow": Notice("overflow", "Mehrere Labormeldungen konnten nicht gesendet werden. Bitte Augur vor Ort pruefen.")}
        self.items[notice.key] = notice

    def flush_one(self, sender):
        if not self.items:
            return
        key = next(iter(self.items))
        sender(self.items[key])
        del self.items[key]  # keep it queued if the send raises an error


def main():
    import tkinter as tk
    from tkinter import filedialog
    print("Augur Laborwache — Dateien beobachten, Messung nicht steuern\n")
    print("Erkennt fehlenden Dateifortschritt und gespeicherte A2-Fehler.")
    print("KEIN Schutz bei PC-/Internetausfall, kein Nachweis gueltiger Messdaten.")
    print("Fenster offen lassen. Vor dem Messlauf starten. Strg+C beendet nur die Wache.\n")
    settings_dir = Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "AugurLabWatch"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "topic.txt"
    topic = settings_file.read_text().strip() if settings_file.exists() else "augur-" + secrets.token_hex(16)
    if not settings_file.exists():
        settings_file.write_text(topic, encoding="utf-8")
    print("iPhone: ntfy installieren, Mitteilungen erlauben, Thema abonnieren.")
    print("Server: https://ntfy.sh\nThema: " + topic + "\n")
    print("Das zufaellige Thema ist kein passwortgeschuetzter Kanal. Nicht weitergeben.")
    input("Nach dem Abonnieren Enter druecken; dann wird eine Testmeldung gesendet. ")
    publish(topic, Notice("test", "Test der Laborwache. Bitte Empfang auch bei gesperrtem iPhone und ueber Mobilfunk pruefen."))
    if input("Test wirklich auf dem iPhone erhalten? 'ja' eingeben: ").strip().lower() != "ja":
        print("Nicht gestartet: Empfang zuerst pruefen.")
        return
    window = tk.Tk()
    window.withdraw()
    try:
        folders = []
        for prompt in ("Aktuellen PD-Aufnahmeordner auswaehlen", "Aktuellen RAW-Aufnahmeordner auswaehlen (darf derselbe sein)"):
            chosen = filedialog.askdirectory(title=prompt, mustexist=True)
            if not chosen:
                print("Ordnerauswahl abgebrochen; keine Ueberwachung gestartet.")
                return
            path = Path(chosen)
            if path not in folders:
                folders.append(path)
    finally:
        window.destroy()
    watch = ProgressWatch(time.monotonic(), snapshot_folders(folders))
    pending = PendingNotices()
    pending.add(Notice("started", "Laborwache gestartet. Alarm nach 60 s ohne RAW- oder PDQ-Dateifortschritt; bitte jetzt den Messlauf starten.", 3))
    read_error = False
    next_send = 0.0
    print("\nUeberwachung aktiv. Kein Alarm bedeutet NICHT, dass die Messung gueltig ist.")
    try:
        while True:
            now = time.monotonic()
            try:
                snapshot = snapshot_folders(folders)
                if read_error:
                    pending.add(Notice("folder-recovered", "Aufnahmeordner wieder erreichbar.", 3))
                read_error = False
                for notice in watch.tick(now, snapshot) + watch.read_changed_sidecars(snapshot):
                    print(time.strftime("%H:%M:%S"), notice.text, flush=True)
                    pending.add(notice)
            except OSError:
                if not read_error:
                    pending.add(Notice("folder-error", "Aufnahmeordner nicht erreichbar. Bitte Laborrechner und Datentraeger pruefen."))
                read_error = True
            if now >= next_send:
                try:
                    pending.flush_one(lambda n: publish(topic, n))
                    next_send = now + 5
                except (OSError, ValueError):
                    print("WARNKANAL NICHT ERREICHBAR. Meldung bleibt ausstehend; keine Fernueberwachung zusichern.", flush=True)
                    next_send = now + 30
            time.sleep(5)
    except KeyboardInterrupt:
        print("\nLaborwache beendet. Augur laeuft unveraendert weiter.")
        try:
            publish(topic, Notice("stopped", "Laborwache wurde beendet. Ab jetzt keine lokalen Warnmeldungen.", 3))
        except (OSError, ValueError):
            print("Abschaltmeldung konnte nicht gesendet werden.")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print("Laborwache NICHT aktiv:", type(error).__name__, str(error))
        input("Enter zum Schliessen. ")
        sys.exit(1)
