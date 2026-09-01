import io
import unittest
from unittest.mock import patch

from mutinydb import MutinyDB


class Response(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class ClientTests(unittest.TestCase):
    def setUp(self):
        self.db = MutinyDB("http://localhost:7654/", "acme")

    @patch("mutinydb.client.urlopen", return_value=Response(b"42\n"))
    def test_register_builds_tenant_scoped_request(self, call):
        self.assertEqual(self.db.register("SELECT * FROM facts"), 42)
        request = call.call_args.args[0]
        self.assertEqual(request.full_url, "http://localhost:7654/v1/acme/sql/register")
        self.assertEqual(request.data, b"SELECT * FROM facts")

    @patch(
        "mutinydb.client.urlopen",
        return_value=Response(b'{"epoch":9,"answer":"row a\\n"}'),
    )
    def test_read_parses_epoch(self, _call):
        answer = self.db.read(4)
        self.assertEqual(answer.epoch, 9)
        self.assertEqual(answer.body, "row a\n")

    @patch(
        "mutinydb.client.urlopen",
        side_effect=lambda *_args, **_kwargs: Response(
            b'[{"rank":1,"key":"evt-7","score":0.875}]'
        ),
    )
    def test_semantic_answer_is_typed_and_spaces_are_percent_encoded(self, call):
        self.assertEqual(self.db.semantic_answer("main", "timeout")[0].key, "evt-7")
        self.db.semantic_answer("main", "tool timed out")
        self.assertIn("query=tool%20timed%20out", call.call_args.args[0].full_url)


if __name__ == "__main__":
    unittest.main()
