"""HTTP and MCP client for the MutinyDB v0.1 surface."""

from dataclasses import dataclass
import json
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode
from urllib.request import Request, urlopen


class MutinyError(RuntimeError):
    """A structured MutinyDB wire error."""

    def __init__(self, status: int, body: str):
        self.status = status
        self.body = body
        self.kind = body.split(None, 1)[0].rstrip(":") if body else "Transport"
        self.retryable = status == 429
        super().__init__(f"MutinyDB {status} {body.strip()}")


@dataclass(frozen=True)
class WriteReceipt:
    commit: int
    epoch: Optional[int]
    rows: int


@dataclass(frozen=True)
class StandingAnswer:
    epoch: int
    body: str


@dataclass(frozen=True)
class SemanticHit:
    rank: int
    key: str
    score: float


@dataclass(frozen=True)
class SemanticGroup:
    group_id: str
    count: int
    avg_cost: float
    error_rate: float
    exemplar_key: str
    member_keys: Tuple[str, ...]


class MutinyDB:
    """A tenant-scoped MutinyDB client.

    Agent-safe operations are available without a token. Operator operations (`taint`,
    `execute`, and shutdown) require the operator token configured by the server.
    """

    def __init__(
        self,
        base_url: str,
        tenant: str,
        operator_token: Optional[str] = None,
        timeout: float = 30.0,
    ) -> None:
        if not tenant or "/" in tenant or ".." in tenant:
            raise ValueError("tenant must be a safe, non-empty identifier")
        self.base_url = base_url.rstrip("/")
        self.tenant = tenant
        self.operator_token = operator_token
        self.timeout = timeout

    def _request(
        self,
        method: str,
        path: str,
        *,
        params: Optional[Mapping[str, Any]] = None,
        body: Any = None,
        operator: bool = False,
    ) -> str:
        # mutinyd's v0.1 query decoder follows URI percent encoding and does not interpret `+` as
        # a space. Force `%20` so semantic text round-trips exactly.
        query = f"?{urlencode(params, quote_via=quote)}" if params else ""
        url = f"{self.base_url}/v1/{quote(self.tenant, safe='')}/{path}{query}"
        headers = {"Accept": "application/json, text/plain"}
        data = None
        if body is not None:
            if isinstance(body, str):
                data = body.encode()
                headers["Content-Type"] = "text/plain; charset=utf-8"
            else:
                data = json.dumps(body, separators=(",", ":")).encode()
                headers["Content-Type"] = "application/json"
        if operator:
            if not self.operator_token:
                raise ValueError("operator_token is required for this operation")
            headers["Authorization"] = f"Bearer {self.operator_token}"
        try:
            with urlopen(
                Request(url, data=data, headers=headers, method=method),
                timeout=self.timeout,
            ) as response:
                return response.read().decode()
        except HTTPError as error:
            raise MutinyError(
                error.code, error.read().decode(errors="replace")
            ) from error
        except URLError as error:
            raise MutinyError(0, str(error.reason)) from error

    def health(self) -> str:
        return self._request("GET", "health")

    def write(
        self,
        *,
        actor: str,
        session: str,
        branch: str,
        intent: str,
        sources: Iterable[Mapping[str, str]],
        table: str,
        rows: Sequence[Sequence[Any]],
    ) -> WriteReceipt:
        text = self._request(
            "POST",
            "write",
            params={"format": "json"},
            body={
                "actor": actor,
                "session": session,
                "branch": branch,
                "intent": intent,
                "sources": list(sources),
                "table": table,
                "rows": rows,
            },
        )
        receipt = json.loads(text)
        return WriteReceipt(receipt["commit"], receipt["epoch"], receipt["rows"])

    def register(self, sql: str, unbounded_reason: Optional[str] = None) -> int:
        params = {"unbounded": unbounded_reason} if unbounded_reason else None
        return int(
            self._request("POST", "sql/register", params=params, body=sql).strip()
        )

    def deregister(self, handle: int) -> None:
        self._request("POST", "sql/deregister", params={"handle": handle})

    def read(self, handle: int) -> StandingAnswer:
        answer = json.loads(
            self._request(
                "GET", "sql/read", params={"handle": handle, "format": "json"}
            )
        )
        return StandingAnswer(answer["epoch"], answer["answer"])

    def oneshot(self, sql: str) -> str:
        return self._request("POST", "query/oneshot", body={"sql": sql})

    def subscribe(
        self, handle: int, from_token: int = 0
    ) -> Tuple[int, List[StandingAnswer]]:
        subscription = json.loads(
            self._request(
                "GET",
                "sql/subscribe",
                params={"handle": handle, "from": from_token, "format": "json"},
            )
        )
        return subscription["token"], [
            StandingAnswer(delta["epoch"], delta["answer"])
            for delta in subscription["deltas"]
        ]

    def plan(self, handle: int) -> str:
        return self._request("GET", "sql/plan", params={"handle": handle})

    def session_open(self, session: str) -> Dict[str, Any]:
        return json.loads(
            self._request("POST", "session/open", body={"session": session})
        )

    def fork(self, session: str, from_branch: str, child: str) -> None:
        self._request(
            "POST",
            "branch/fork",
            body={"session": session, "from": from_branch, "child": child},
        )

    def merge(self, session: str, child: str, into: str) -> int:
        return int(
            self._request(
                "POST",
                "branch/merge",
                body={"session": session, "child": child, "into": into},
            )
            .strip()
            .removeprefix("merged ")
        )

    def rewind(self, session: str, child: str) -> int:
        return int(
            self._request(
                "POST", "branch/rewind", body={"session": session, "child": child}
            )
            .strip()
            .removeprefix("freed ")
        )

    def semantic_answer(self, branch: str, query: str) -> List[SemanticHit]:
        hits = json.loads(
            self._request(
                "GET",
                "semantic/answer",
                params={"branch": branch, "query": query, "format": "json"},
            )
        )
        return [SemanticHit(hit["rank"], hit["key"], hit["score"]) for hit in hits]

    def semantic_groups(self, branch: str, group: str) -> List[SemanticGroup]:
        groups = json.loads(
            self._request(
                "GET",
                "semantic/groups",
                params={"branch": branch, "group": group, "format": "json"},
            )
        )
        return [
            SemanticGroup(
                item["group_id"],
                item["count"],
                item["avg_cost"],
                item["error_rate"],
                item["exemplar_key"],
                tuple(item["member_keys"]),
            )
            for item in groups
        ]

    def propose(
        self,
        *,
        actor: str,
        branch: str,
        action_type: str,
        target: str,
        idempotency_key: str,
        justified_by: Sequence[str] = (),
    ) -> str:
        text = self._request(
            "POST",
            "action/propose",
            body={
                "actor": actor,
                "branch": branch,
                "action_type": action_type,
                "target": target,
                "idempotency_key": idempotency_key,
                "justified_by": list(justified_by),
            },
        )
        return text.strip().removeprefix("proposed ")

    def execute(self, proposal: str) -> str:
        return self._request(
            "POST", "action/execute", body={"proposal": proposal}, operator=True
        )

    def taint(self, system: str, record: str) -> str:
        return self._request(
            "POST", "taint", body={"system": system, "record": record}, operator=True
        )

    def mcp_call(
        self, name: str, arguments: Mapping[str, Any], request_id: Any = 1
    ) -> Any:
        text = self._request(
            "POST",
            "mcp",
            body={
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            },
        )
        response = json.loads(text)
        if "error" in response:
            raise MutinyError(200, response["error"].get("message", "MCP error"))
        return response["result"].get("structured")
