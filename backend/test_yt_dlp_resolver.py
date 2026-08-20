import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from yt_dlp_resolver import (
    ResolverService,
    resolved_media,
    resolved_playlist,
    resolved_search_media,
    safe_headers,
)


class FakeYdl:
    def __init__(self, options):
        self.options = options
        self.params = dict(options)
        self.calls = 0
        self.closed = False

    def extract_info(self, url, download):
        self.calls += 1
        return {
            "url": f"https://media.example/{self.calls}",
            "protocol": "https",
            "duration": 42,
            "http_headers": {
                "User-Agent": "Pocket Test",
                "Cookie": "must-not-leak=yes",
            },
        }

    def close(self):
        self.closed = True


class ResolverTests(unittest.TestCase):
    def test_sensitive_headers_are_not_returned(self):
        self.assertEqual(
            safe_headers({"User-Agent": "test", "Cookie": "secret"}),
            {"User-Agent": "test"},
        )

    def test_resolved_media_requires_direct_http(self):
        with self.assertRaises(ValueError):
            resolved_media({"url": "manifest", "protocol": "m3u8_native"})

    def test_search_result_selects_the_first_direct_video(self):
        result = resolved_search_media(
            {
                "entries": [
                    {"id": "missing", "title": "No stream"},
                    {
                        "id": "video-id",
                        "title": "Matching upload",
                        "url": "https://media.example/audio",
                        "protocol": "https",
                        "duration": 249,
                    },
                ]
            }
        )

        self.assertEqual(result["videoId"], "video-id")
        self.assertEqual(result["title"], "Matching upload")

    def test_playlist_result_normalizes_flat_video_entries(self):
        result = resolved_playlist(
            {
                "id": "RDmix",
                "entries": [
                    {
                        "id": "video-id",
                        "title": "First song",
                        "channel": "Artist",
                        "duration": 236.4,
                        "thumbnails": [{"url": "small"}, {"url": "large"}],
                    },
                    {"id": "", "title": "Missing video"},
                ],
            },
            "requested",
        )

        self.assertEqual(result["playlistId"], "RDmix")
        self.assertEqual(len(result["items"]), 1)
        self.assertEqual(result["items"][0]["videoId"], "video-id")
        self.assertEqual(result["items"][0]["sourcePlaylistId"], "RDmix")
        self.assertEqual(result["items"][0]["sourceIndex"], 0)
        self.assertEqual(result["items"][0]["thumbnail"], "large")
        self.assertEqual(result["items"][0]["durationSeconds"], 236)

    def test_public_playlist_is_flattened_and_cached(self):
        class FakePlaylistYdl(FakeYdl):
            def extract_info(self, url, download):
                self.calls += 1
                self.last_url = url
                return {
                    "id": "RDmix",
                    "entries": [
                        {
                            "id": "video-id",
                            "title": "First song",
                            "duration": 42,
                        }
                    ],
                }

        instances = []

        def factory(options):
            instance = FakePlaylistYdl(options)
            instances.append(instance)
            return instance

        service = ResolverService(None, None, factory)
        first = service.playlist_resolve("RDmix", 12)
        second = service.playlist_resolve("RDmix", 12)

        self.assertEqual(first, second)
        self.assertEqual(len(instances), 1)
        self.assertEqual(instances[0].calls, 1)
        self.assertEqual(instances[0].options["extract_flat"], "in_playlist")
        self.assertEqual(instances[0].options["playlistend"], 12)
        self.assertEqual(
            instances[0].last_url,
            "https://www.youtube.com/playlist?list=RDmix",
        )

    def test_alternative_youtube_search_is_cached(self):
        class FakeSearchYdl(FakeYdl):
            def extract_info(self, url, download):
                self.calls += 1
                self.last_url = url
                return {
                    "entries": [
                        {
                            "id": "replacement",
                            "title": "Replacement upload",
                            "url": "https://media.example/replacement",
                            "protocol": "https",
                            "duration": 42,
                        }
                    ]
                }

        instances = []

        def factory(options):
            instance = FakeSearchYdl(options)
            instances.append(instance)
            return instance

        service = ResolverService(None, None, factory)
        first = service.search_resolve("song artist", "bestaudio/best")
        second = service.search_resolve("song artist", "bestaudio/best")

        self.assertEqual(first, second)
        self.assertEqual(instances[0].last_url, "ytsearch1:song artist")
        self.assertEqual(instances[0].calls, 1)

    def test_successful_resolutions_are_cached_for_the_session(self):
        instances = []

        def factory(options):
            instance = FakeYdl(options)
            instances.append(instance)
            return instance

        service = ResolverService(None, None, factory)
        first = service.resolve("https://youtube.test/watch?v=one", "bestaudio/best")
        second = service.resolve("https://youtube.test/watch?v=one", "bestaudio/best")

        self.assertEqual(first, second)
        self.assertEqual(instances[0].calls, 1)
        self.assertEqual(first["headers"], {"User-Agent": "Pocket Test"})

    def test_ping_warms_the_default_yt_dlp_instance_without_network(self):
        instances = []

        def factory(options):
            instance = FakeYdl(options)
            instances.append(instance)
            return instance

        service = ResolverService(None, None, factory)
        response = service.handle({"op": "ping"})

        self.assertTrue(response["ready"])
        self.assertEqual(len(instances), 1)
        self.assertEqual(instances[0].options["format"], "bestaudio/best")
        self.assertEqual(instances[0].calls, 0)

    def test_audio_transport_does_not_attach_account_cookies(self):
        with TemporaryDirectory() as directory:
            cookies = Path(directory) / "cookies.txt"
            cookies.write_text("cookie", encoding="utf-8")
            instances = []

            def factory(options):
                instance = FakeYdl(options)
                instances.append(instance)
                return instance

            service = ResolverService(None, str(cookies), factory)
            service.resolve("https://www.youtube.com/watch?v=public", "bestaudio/best")

            self.assertNotIn("cookiefile", instances[0].options)
            self.assertEqual(
                instances[0].options["extractor_args"],
                {"youtube": {"player_client": ["web_embedded"]}},
            )

    def test_repeatable_embedded_and_android_profiles_are_exposed(self):
        with TemporaryDirectory() as directory:
            cookies = Path(directory) / "cookies.txt"
            cookies.write_text("cookie", encoding="utf-8")
            instances = []

            def factory(options):
                instance = FakeYdl(options)
                instances.append(instance)
                return instance

            service = ResolverService(None, str(cookies), factory)
            embedded = service.resolve(
                "https://www.youtube.com/watch?v=public", "bestaudio/best", 0
            )
            cached_embedded = service.resolve(
                "https://www.youtube.com/watch?v=public", "bestaudio/best", 0
            )
            android = service.resolve(
                "https://www.youtube.com/watch?v=public", "bestaudio/best", 1
            )

            self.assertEqual(len(instances), 2)
            self.assertNotIn("cookiefile", instances[0].options)
            self.assertNotIn("cookiefile", instances[1].options)
            self.assertEqual(
                instances[0].options["extractor_args"],
                {"youtube": {"player_client": ["web_embedded"]}},
            )
            self.assertEqual(
                instances[1].options["extractor_args"],
                {"youtube": {"player_client": ["android"]}},
            )
            self.assertEqual(embedded["url"], "https://media.example/1")
            self.assertEqual(embedded, cached_embedded)
            self.assertEqual(android["url"], "https://media.example/1")

    def test_profile_outside_available_candidates_is_rejected(self):
        service = ResolverService(None, None, FakeYdl)
        with self.assertRaisesRegex(ValueError, "재생 프로필"):
            service.resolve("https://www.youtube.com/watch?v=public", "bestaudio/best", 2)

    def test_po_provider_does_not_replace_the_repeatable_primary_profile(self):
        with TemporaryDirectory() as directory:
            provider = Path(directory) / "provider"
            provider.mkdir()
            instances = []

            def factory(options):
                instance = FakeYdl(options)
                instances.append(instance)
                return instance

            service = ResolverService(None, None, factory, str(provider))
            service.resolve(
                "https://www.youtube.com/watch?v=public", "bestaudio/best", 0
            )

            self.assertEqual(
                instances[0].options["extractor_args"],
                {"youtube": {"player_client": ["web_embedded"]}},
            )
            self.assertNotIn("cookiefile", instances[0].options)


if __name__ == "__main__":
    unittest.main()
