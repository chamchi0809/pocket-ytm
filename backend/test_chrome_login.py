import sqlite3
import tempfile
import unittest
from pathlib import Path

from chrome_login import (
    BrowseRequestCollector,
    capture_command,
    has_youtube_login_cookie,
    headers_as_text,
    interactive_login_command,
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

    def test_headers_require_authenticated_browser_values(self) -> None:
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

    def test_interactive_login_has_no_automation_flags(self) -> None:
        command = interactive_login_command("/browser", Path("/profile"))

        self.assertFalse(any("remote-debugging" in argument for argument in command))
        self.assertFalse(any("headless" in argument for argument in command))
        self.assertFalse(any("load-extension" in argument for argument in command))

    def test_capture_starts_headless_on_a_blank_page(self) -> None:
        command = capture_command("/browser", Path("/profile"))

        self.assertIn("--headless=new", command)
        self.assertIn("--remote-debugging-port=0", command)
        self.assertEqual(command[-1], "about:blank")

    def test_login_cookie_is_detected_without_decrypting_it(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory)
            database = profile / "Default/Network/Cookies"
            database.parent.mkdir(parents=True)
            connection = sqlite3.connect(database)
            connection.execute("CREATE TABLE cookies (host_key TEXT, name TEXT)")
            connection.execute(
                "INSERT INTO cookies VALUES (?, ?)",
                (".youtube.com", "__Secure-3PAPISID"),
            )
            connection.commit()
            connection.close()

            self.assertTrue(has_youtube_login_cookie(profile))

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
                        "url": "https://music.youtube.com/youtubei/v1/browse",
                        "method": "POST",
                        "headers": {"X-Goog-AuthUser": "0"},
                    },
                },
            }
        )

        self.assertIn("authorization: SAPISIDHASH value", captured)
        self.assertIn("cookie: SID=one", captured)

    def test_collector_discards_headers_for_other_requests(self) -> None:
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
