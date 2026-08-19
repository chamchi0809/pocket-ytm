import unittest

from yt_dlp_resolver import ResolverService, resolved_media, safe_headers


class FakeYdl:
    def __init__(self, options):
        self.options = options
        self.params = dict(options)
        self.calls = 0

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
        pass


class ResolverTests(unittest.TestCase):
    def test_sensitive_headers_are_not_returned(self):
        self.assertEqual(
            safe_headers({"User-Agent": "test", "Cookie": "secret"}),
            {"User-Agent": "test"},
        )

    def test_resolved_media_requires_direct_http(self):
        with self.assertRaises(ValueError):
            resolved_media({"url": "manifest", "protocol": "m3u8_native"})

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


if __name__ == "__main__":
    unittest.main()
