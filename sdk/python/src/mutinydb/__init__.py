"""Typed, zero-dependency MutinyDB client."""

from .client import (
    MutinyDB,
    MutinyError,
    SemanticGroup,
    SemanticHit,
    StandingAnswer,
    WriteReceipt,
)

__all__ = [
    "MutinyDB",
    "MutinyError",
    "SemanticGroup",
    "SemanticHit",
    "StandingAnswer",
    "WriteReceipt",
]
__version__ = "0.1.0"
