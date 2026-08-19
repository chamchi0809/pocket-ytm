import unittest

from ytmusic_bridge import (
    Service,
    item_thumbnail,
    normalize_auth_input,
    normalize_item,
    thumbnail,
)


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


class NormalizeMediaItemTests(unittest.TestCase):
    def test_thumbnail_avoids_oversized_texture_candidates(self) -> None:
        value = thumbnail(
            {
                "thumbnails": [
                    {"url": "small", "width": 120, "height": 120},
                    {"url": "right-sized", "width": 400, "height": 400},
                    {"url": "oversized", "width": 1200, "height": 1200},
                ]
            }
        )

        self.assertEqual(value, "right-sized")

    def test_explore_album_watch_playlist_is_routed_to_album_browse(self) -> None:
        item = normalize_item(
            {
                "title": "Still Here",
                "playlistId": "VLMPREb_8YVkk5tppvW",
                "thumbnails": [],
            }
        )

        self.assertIsNotNone(item)
        self.assertEqual(item["kind"], "album")
        self.assertEqual(item["browseId"], "MPREb_8YVkk5tppvW")

    def test_watch_queue_length_is_preserved_for_end_detection(self) -> None:
        item = normalize_item(
            {"title": "Queue track", "videoId": "video", "length": "3:29"},
            "song",
        )

        self.assertEqual(item["durationSeconds"], 209)

    def test_track_thumbnail_falls_back_to_the_video_endpoint(self) -> None:
        self.assertEqual(
            item_thumbnail({"videoId": "abc_123-Z"}, "abc_123-Z"),
            "https://i.ytimg.com/vi/abc_123-Z/hqdefault.jpg",
        )
        self.assertIsNone(item_thumbnail({}, "not/a/video/id"))


class LibraryOrderingTests(unittest.TestCase):
    def test_playlists_are_the_first_library_section(self) -> None:
        class FakeYtMusic:
            @staticmethod
            def get_library_playlists(limit: int):
                return [{"title": "내 플리", "playlistId": "PL-test"}]

            @staticmethod
            def get_library_songs(limit: int):
                return [{"title": "노래", "videoId": "video-test"}]

            @staticmethod
            def get_library_albums(limit: int):
                return [{"title": "앨범", "browseId": "MPRE-test"}]

            @staticmethod
            def get_library_artists(limit: int):
                return [{"artist": "가수", "browseId": "UC-test"}]

        service = Service.__new__(Service)
        service.authenticated = True
        service.yt = FakeYtMusic()

        sections = service.dispatch("library", {"category": "all", "limit": 10})

        self.assertEqual(sections[0]["title"], "플레이리스트")


class SavedAuthenticationTests(unittest.TestCase):
    def test_cached_account_status_does_not_make_a_network_request(self) -> None:
        class FakeYtMusic:
            def __getattr__(self, _name):
                raise AssertionError("account status must use only cached metadata")

        service = Service.__new__(Service)
        service.authenticated = True
        service.account = {"accountName": "저장된 계정"}
        service.account_path = None
        service.yt = FakeYtMusic()

        status = service.account_status()

        self.assertTrue(status["authenticated"])
        self.assertEqual(status["name"], "저장된 계정")

    def test_saved_auth_is_optimistically_restored_until_an_api_request_fails(self) -> None:
        service = Service.__new__(Service)
        service.authenticated = True
        service.account = {"accountName": "오래된 계정"}
        service.account_path = None

        status = service.account_status()

        self.assertTrue(status["authenticated"])
        self.assertEqual(status["name"], "오래된 계정")


if __name__ == "__main__":
    unittest.main()
