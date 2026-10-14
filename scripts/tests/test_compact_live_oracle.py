#!/usr/bin/env python3
"""Reject blind spots in the loopback oracle's request classification."""

import unittest

from compact_live_oracle import SUMMARY_PREFIX, is_summary


class CompactCaptureTests(unittest.TestCase):
    def test_both_anthropic_content_forms_identify_the_summary_request(self):
        for content in [SUMMARY_PREFIX + " summary", [{"type": "text", "text": SUMMARY_PREFIX}]]:
            self.assertTrue(is_summary({"messages": [{"role": "user", "content": content}]}))

    def test_unrelated_or_malformed_content_is_not_a_summary_request(self):
        for content in ["ordinary prompt", [], None, 42, [{"text": None}], {"text": SUMMARY_PREFIX}]:
            self.assertFalse(is_summary({"messages": [{"role": "user", "content": content}]}))


if __name__ == "__main__":
    unittest.main()
