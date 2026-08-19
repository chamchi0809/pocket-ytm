import unittest

from chrome_login import (
    BrowseRequestCollector,
    headers_as_text,
    is_music_browse_request,
)


class ChromeLoginTests(unittest.TestCase):
    def test_only_music_browse_post_requests_are_accepted(self) -> None:
        self.assertTrue(
            is_music_browse_request(
                "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false",
                "POST",
            )
        )
        self.assertFalse(
            is_music_browse_request(
                "https://music.youtube.com/youtubei/v1/search", "POST"
            )
        )
        self.assertFalse(
            is_music_browse_request(
                "https://accounts.google.com/youtubei/v1/browse", "POST"
            )
        )

    def test_headers_require_the_authenticated_browser_values(self) -> None:
        self.assertIsNone(headers_as_text({"Cookie": "SID=one"}))
        captured = headers_as_text(
            {
                "Authorization": "SAPISIDHASH value",
                "Cookie": "SID=one",
                "X-Goog-AuthUser": "0",
            }
        )

        self.assertIn("authorization: SAPISIDHASH value", captured)
        self.assertIn("cookie: SID=one", captured)
        self.assertIn("x-goog-authuser: 0", captured)

    def test_collector_correlates_extra_headers_arriving_first(self) -> None:
        collector = BrowseRequestCollector()
        self.assertIsNone(
            collector.ingest(
                {
                    "method": "Network.requestWillBeSentExtraInfo",
                    "params": {
                        "requestId": "request-1",
                        "headers": {
                            "Cookie": "SID=one",
                            "Authorization": "SAPISIDHASH value",
                        },
                    },
                }
            )
        )

        captured = collector.ingest(
            {
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "request-1",
                    "request": {
                        "url": "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false",
                        "method": "POST",
                        "headers": {"X-Goog-AuthUser": "0"},
                    },
                },
            }
        )

        self.assertIn("authorization: SAPISIDHASH value", captured)
        self.assertIn("cookie: SID=one", captured)
        self.assertIn("x-goog-authuser: 0", captured)

    def test_collector_correlates_request_arriving_first(self) -> None:
        collector = BrowseRequestCollector()
        self.assertIsNone(
            collector.ingest(
                {
                    "method": "Network.requestWillBeSent",
                    "params": {
                        "requestId": "request-2",
                        "request": {
                            "url": "https://music.youtube.com/youtubei/v1/browse",
                            "method": "POST",
                            "headers": {
                                "Authorization": "SAPISIDHASH value",
                                "X-Goog-AuthUser": "0",
                            },
                        },
                    },
                }
            )
        )

        captured = collector.ingest(
            {
                "method": "Network.requestWillBeSentExtraInfo",
                "params": {
                    "requestId": "request-2",
                    "headers": {"Cookie": "SID=one"},
                },
            }
        )

        self.assertIn("authorization: SAPISIDHASH value", captured)
        self.assertIn("cookie: SID=one", captured)
        self.assertIn("x-goog-authuser: 0", captured)

    def test_collector_discards_extra_headers_for_other_requests(self) -> None:
        collector = BrowseRequestCollector()
        collector.ingest(
            {
                "method": "Network.requestWillBeSentExtraInfo",
                "params": {
                    "requestId": "other-request",
                    "headers": {"Cookie": "temporary"},
                },
            }
        )
        collector.ingest(
            {
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "other-request",
                    "request": {
                        "url": "https://accounts.google.com/login",
                        "method": "POST",
                        "headers": {},
                    },
                },
            }
        )

        self.assertNotIn("other-request", collector._headers)


if __name__ == "__main__":
    unittest.main()
