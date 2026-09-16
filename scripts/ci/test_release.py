"""Ensure failed publication checks preserve existing Releases."""
import unittest
from unittest.mock import patch

import release


class ReleaseTests(unittest.TestCase):
    def test_failed_verification_never_publishes_or_deletes(self):
        with patch.object(release, 'find_release', return_value={'id': 2, 'draft': True}), \
                patch.object(release, 'verify', side_effect=RuntimeError('bad digest')), \
                patch.object(release, 'api') as api:
            with self.assertRaisesRegex(RuntimeError, 'bad digest'):
                release.publish('v0.1.2')
            api.assert_not_called()

    def test_unconfirmed_latest_preserves_older_releases(self):
        candidate = {'id': 2, 'draft': True}
        with patch.object(release, 'find_release', return_value=candidate), \
                patch.object(release, 'verify'), \
                patch.object(release, 'api', side_effect=[None, {'id': 1, 'draft': False}]) as api:
            with self.assertRaisesRegex(RuntimeError, 'older Releases were retained'):
                release.publish('v0.1.2')
            self.assertFalse(any(call.args[1:] == ('DELETE',) for call in api.call_args_list))

    def test_publish_verifies_new_latest_before_deleting_old_release(self):
        candidate = {'id': 2, 'draft': True}
        latest = {'id': 2, 'draft': False}
        old = {'id': 1, 'draft': False, 'tag_name': 'v0.1.1'}
        another_draft = {'id': 3, 'draft': True}
        events = []

        def api(path, method='GET', data=None):
            events.append((path, method))
            return latest if path == 'releases/latest' else None

        with patch.object(release, 'find_release', return_value=candidate), \
                patch.object(release, 'verify', side_effect=lambda r: events.append(('verify', r['draft']))), \
                patch.object(release, 'api', side_effect=api), \
                patch.object(release, 'releases', return_value=[[old, latest, another_draft]]):
            release.publish('v0.1.2')
        self.assertEqual(events, [('verify', True), ('releases/2', 'PATCH'),
                                  ('releases/latest', 'GET'), ('verify', False),
                                  ('releases/1', 'DELETE')])

    def test_existing_public_release_cannot_be_overwritten_by_build(self):
        with patch.dict(release.os.environ, {'GITHUB_SHA': 'test-sha'}), \
                patch.object(release, 'find_release', return_value={'draft': False}), \
                patch.object(release, 'gh') as gh:
            with self.assertRaisesRegex(RuntimeError, 'another release'):
                release.draft('v0.1.1')
            gh.assert_not_called()


if __name__ == '__main__':
    unittest.main()
