#!/usr/bin/env python3
import unittest
from protocol_process_rss import parse_processes


class ProcessRssTests(unittest.TestCase):
    def test_shared_process_pages_are_not_added_into_a_fake_physical_total(self):
        rows = parse_processes('PID COMMAND RSS NLWP\n1 nginx 2048 1\n2 nginx 3072 1\n')
        self.assertEqual([row['rss_kib'] for row in rows], [2048, 3072])
        self.assertEqual(len(rows), 2)

    def test_rejects_wrong_units_columns_and_duplicate_processes(self):
        for raw in ['PID COMMAND VSZ NLWP\n1 nginx 2048 1',
                    'PID COMMAND RSS NLWP\n1 nginx 2048 1\n1 nginx 3072 1',
                    'PID COMMAND RSS NLWP\n1 nginx -1 1']:
            with self.assertRaises(AssertionError):
                parse_processes(raw)


if __name__ == '__main__':
    unittest.main()
