from django.test import SimpleTestCase
from django.urls import reverse


class HelloViewTests(SimpleTestCase):
    def test_index_returns_hello_world(self) -> None:
        response = self.client.get(reverse("index"))

        self.assertEqual(response.status_code, 200)
        self.assertContains(response, "Hello world")
