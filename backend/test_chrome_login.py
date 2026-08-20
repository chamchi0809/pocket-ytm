import sqlite3
import tempfile
import unittest
from hashlib import sha1
from pathlib import Path

from chrome_login import (
    ChromeLoginError,
    auth_headers_from_cookies,
    capture_command,
    has_completed_google_login,
    has_youtube_login_cookie,
    interactive_login_command,
)


class ChromeLoginTests(unittest.TestCase):
    def test_auth_headers_are_built_from_cdp_cookies(self) -> None:
        timestamp = 1_700_000_000
        sapisid = "secret-value"
        captured = auth_headers_from_cookies(
            [
                {"name": "PREF", "value": "hl=ko"},
                {"name": "__Secure-3PAPISID", "value": sapisid},
            ],
            timestamp,
        )
        digest = sha1(
            f"{timestamp} {sapisid} https://music.youtube.com".encode()
        ).hexdigest()

        self.assertIn(f"authorization: SAPISIDHASH {timestamp}_{digest}", captured)
        self.assertIn("cookie: PREF=hl=ko; __Secure-3PAPISID=secret-value", captured)
        self.assertIn("x-goog-authuser: 0", captured)

    def test_auth_headers_require_login_cookie(self) -> None:
        with self.assertRaises(ChromeLoginError):
            auth_headers_from_cookies([{"name": "PREF", "value": "hl=ko"}])

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

    def test_return_to_music_after_google_login_is_detected_from_history(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory)
            database = profile / "Default/History"
            database.parent.mkdir(parents=True)
            connection = sqlite3.connect(database)
            connection.execute("CREATE TABLE urls (url TEXT, last_visit_time INTEGER)")
            connection.executemany(
                "INSERT INTO urls VALUES (?, ?)",
                [
                    ("https://music.youtube.com/library", 100),
                    ("https://accounts.google.com/v3/signin", 200),
                ],
            )
            connection.commit()

            self.assertFalse(has_completed_google_login(profile))

            connection.execute(
                "INSERT INTO urls VALUES (?, ?)",
                ("https://music.youtube.com/library", 300),
            )
            connection.commit()
            connection.close()

            self.assertTrue(has_completed_google_login(profile))

    def test_non_premium_redirect_to_youtube_is_detected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory)
            database = profile / "Default/History"
            database.parent.mkdir(parents=True)
            connection = sqlite3.connect(database)
            connection.execute("CREATE TABLE urls (url TEXT, last_visit_time INTEGER)")
            connection.executemany(
                "INSERT INTO urls VALUES (?, ?)",
                [
                    ("https://music.youtube.com/library", 100),
                    ("https://www.youtube.com/musicpremium", 110),
                    ("https://accounts.google.com/v3/signin", 200),
                ],
            )
            connection.commit()

            self.assertFalse(has_completed_google_login(profile))

            connection.execute(
                "UPDATE urls SET last_visit_time = 300 WHERE url = ?",
                ("https://www.youtube.com/musicpremium",),
            )
            connection.commit()
            connection.close()

            self.assertTrue(has_completed_google_login(profile))

    def test_history_without_a_google_login_is_not_complete(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory)
            database = profile / "Default/History"
            database.parent.mkdir(parents=True)
            connection = sqlite3.connect(database)
            connection.execute("CREATE TABLE urls (url TEXT, last_visit_time INTEGER)")
            connection.execute(
                "INSERT INTO urls VALUES (?, ?)",
                ("https://music.youtube.com/library", 100),
            )
            connection.commit()
            connection.close()

            self.assertFalse(has_completed_google_login(profile))

if __name__ == "__main__":
    unittest.main()
