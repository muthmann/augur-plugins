import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('watch', Path(__file__).with_name('stage_a_watch.py'))
w = importlib.util.module_from_spec(spec)
sys.modules['watch'] = w
spec.loader.exec_module(w)


class WatchTests(unittest.TestCase):
    def test_unchanged_old_files_do_not_count_as_progress(self):
        files = {'old.raw': (100, 1), 'old.pdq': (100, 1)}
        watch = w.ProgressWatch(0, files)
        self.assertEqual(len(watch.tick(61, files)), 2)
        self.assertFalse(watch.active)

    def test_both_streams_are_checked_independently(self):
        watch = w.ProgressWatch(0, {})
        self.assertEqual(watch.tick(1, {'x.raw': (10, 1)}), [])
        alerts = watch.tick(62, {'x.raw': (20, 2)})
        self.assertEqual([a.key for a in alerts], ['stalled:.pdq'])

    def test_timestamp_touch_does_not_mask_a_stall(self):
        watch = w.ProgressWatch(0, {'x.raw': (10, 1)})
        self.assertIn('stalled:.raw', [n.key for n in watch.tick(61, {'x.raw': (10, 2)})])

    def test_rotation_is_progress_and_no_alarm_spam(self):
        watch = w.ProgressWatch(0, {})
        files = {'x.raw': (100, 1), 'x.pdq': (100, 1)}
        watch.tick(1, files)
        self.assertEqual(watch.tick(59, {**files, 'y.raw': (1, 2), 'y.pdq': (1, 2)}), [])
        self.assertEqual(len(watch.tick(120, watch.previous)), 2)
        self.assertEqual(watch.tick(130, watch.previous), [])
        self.assertEqual(len(watch.tick(131, {**watch.previous, 'z.raw': (1, 3), 'z.pdq': (1, 3)})), 2)

    def test_zero_length_files_are_not_progress(self):
        watch = w.ProgressWatch(0, {})
        self.assertEqual(len(watch.tick(61, {'x.raw': (0, 1)})), 2)

    def test_pending_alert_survives_network_failure(self):
        queue = w.PendingNotices()
        queue.add(w.Notice('a', 'test'))
        def fail(_): raise OSError('offline')
        with self.assertRaises(OSError): queue.flush_one(fail)
        self.assertIn('a', queue.items)
        got = []
        queue.flush_one(got.append)
        self.assertEqual(len(got), 1)
        self.assertFalse(queue.items)

    def test_recovery_cancels_unsent_stall(self):
        queue = w.PendingNotices()
        queue.add(w.Notice('stalled:.pdq', 'stall'))
        queue.add(w.Notice('resumed:.pdq', 'resumed'))
        self.assertNotIn('stalled:.pdq', queue.items)

    def test_partial_json_is_retried_and_errors_do_not_upload_local_paths(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 'x.a2.json'
            p.write_text('{')
            watch = w.ProgressWatch(0, {})
            snapshot = w.snapshot_folders([Path(d)])
            self.assertEqual(watch.read_changed_sidecars(snapshot), [])
            p.write_text(json.dumps({'experiment': 'A2', 'protocol_row': 2, 'evidence': {'failure': 'secret local path'}}))
            alerts = watch.read_changed_sidecars(w.snapshot_folders([Path(d)]))
            self.assertEqual(len(alerts), 1)
            self.assertNotIn('secret', alerts[0].text)
            self.assertEqual(watch.read_changed_sidecars(w.snapshot_folders([Path(d)])), [])

    def test_non_object_json_is_ignored_without_crashing_the_watcher(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 'x.a2.json'
            p.write_text('[]')
            self.assertEqual(w.ProgressWatch(0, {}).read_changed_sidecars(w.snapshot_folders([Path(d)])), [])

    def test_missing_folder_is_an_error_not_empty_healthy_state(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(OSError): w.snapshot_folders([Path(d) / 'missing'])

    def test_publish_validates_topic_and_uses_bounded_http(self):
        with self.assertRaises(ValueError): w.publish('../escape', w.Notice('x', 'x'))
        calls = []
        class Response:
            status = 200
            def __enter__(self): return self
            def __exit__(self, *args): pass
        def opener(request, **kwargs):
            calls.append((request, kwargs));return Response()
        w.publish('augur-0123456789abcdef', w.Notice('x', 'Test'), opener)
        request, kwargs = calls[0]
        self.assertEqual(request.full_url, 'https://ntfy.sh/augur-0123456789abcdef')
        self.assertEqual(kwargs['timeout'], 8)
        self.assertEqual(request.data, b'Test')


if __name__ == '__main__': unittest.main()
