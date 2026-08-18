import unittest

from ytmusic_bridge import normalize_auth_input


class NormalizeAuthInputTests(unittest.TestCase):
    def test_chrome_node_fetch_extracts_sensitive_headers_without_executing_code(self) -> None:
        copied = r'''fetch("https://music.youtube.com/youtubei/v1/browse?prettyPrint=false", {
          "headers": {
            "accept": "*/*",
            "authorization": "SAPISIDHASH 123_test",
            "cookie": "SID=alpha; TOKEN=beta=two",
            "x-goog-authuser": "0"
          },
          "body": "{\"context\":{}}",
          "method": "POST"
        });'''

        normalized = normalize_auth_input(copied)

        self.assertIn("authorization: SAPISIDHASH 123_test", normalized)
        self.assertIn("cookie: SID=alpha; TOKEN=beta=two", normalized)
        self.assertIn("x-goog-authuser: 0", normalized)
        self.assertNotIn("body", normalized)

    def test_raw_request_headers_remain_supported(self) -> None:
        copied = "cookie: SID=alpha\nx-goog-authuser: 0\nauthorization: SAPISIDHASH hash"

        self.assertEqual(normalize_auth_input(copied), copied)

    def test_fetch_from_another_origin_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "music.youtube.com"):
            normalize_auth_input('fetch("https://example.com", {"headers": {}});')


if __name__ == "__main__":
    unittest.main()
