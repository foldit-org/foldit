"""Dummy plugin — smoke test for the unified plugin protocol.

Returns a fixed alanine PDB for predict/design (ops) and fixed sequences
for sequence_design (query), plus a trivial streaming op exercising the
pending/checkpoint/cancelled/final poll outcomes. No real ML; used to
exercise the worker host and orchestrator dispatch paths end-to-end.
"""

from __future__ import annotations

from typing import Any

from foldit_plugin_sdk import PluginInterface, DispatchContext, PollOutcome, make_param_value
from foldit_plugin_sdk.logging_config import get_logger
from foldit_plugin_sdk.proto import plugin_pb2

logger = get_logger(__name__)


_ALANINE_PDB = (
    "ATOM      1  N   ALA A   1       0.000   0.000   0.000  1.00  0.00           N\n"
    "ATOM      2  CA  ALA A   1       1.458   0.000   0.000  1.00  0.00           C\n"
    "ATOM      3  C   ALA A   1       2.009   1.420   0.000  1.00  0.00           C\n"
    "ATOM      4  O   ALA A   1       1.251   2.390   0.000  1.00  0.00           O\n"
    "ATOM      5  CB  ALA A   1       2.009  -0.773  -1.232  1.00  0.00           C\n"
    "TER\n"
)


def _alanine_assembly_bytes() -> bytes:
    """Alanine as molex assembly wire-format bytes.

    Streaming terminals (final/cancelled) and checkpoint snapshots are
    deserialized by the orchestrator as a `molex::Assembly`; raw PDB text
    is not the wire format, so it must round-trip through molex first.
    """
    import molex

    return molex.pdb_to_assembly_bytes(_ALANINE_PDB)


class Plugin(PluginInterface):
    """Dummy plugin. Two invoke ops, one stream op, one query. No real ML.

    Ops (return assembly bytes):

    - ``predict(sequence: STRING, num_recycles: INT)`` — INVOKE, returns alanine PDB.
    - ``design(length: STRING, contig: STRING, num_designs: INT, save_trajectories: BOOL)``
      — INVOKE, returns alanine PDB.
    - ``stream_test()`` — STREAM. Emits two ``pending`` polls, one
      ``checkpoint``, then a ``final`` carrying the alanine PDB; a host
      cancel flips it to a ``cancelled`` terminal.

    Queries (return query-defined data bytes):

    - ``sequence_design(temperature: FLOAT, num_sequences: INT)`` — returns
      newline-separated ``"sequence\\tscore"`` lines.
    """

    def __init__(self, config: dict[str, Any]) -> None:
        self.config = config
        self._assembly: bytes | None = None
        # Per-request_id stream state: poll counter + a cancel flag.
        self._streams: dict[int, dict[str, Any]] = {}
        logger.info("Initialized with config: %s", config)

    def init(self, assembly_bytes: bytes) -> int:
        self._assembly = assembly_bytes
        logger.info("init: %d bytes assembly received", len(assembly_bytes))
        return 1

    def update_assembly(
        self,
        session: int,
        payload_kind: int,
        bytes: bytes,
        from_gen: int,
        to_gen: int,
    ) -> None:
        # Dummy plugin treats both Full and Delta payloads as opaque
        # blobs — it doesn't actually parse the Assembly.
        del session, payload_kind, from_gen, to_gen
        self._assembly = bytes
        logger.debug("update_assembly: %d bytes", len(bytes))

    def drop(self, session: int) -> None:
        self._assembly = None
        logger.info("drop session %d", session)

    def register(self) -> plugin_pb2.PluginRegistration:
        return plugin_pb2.PluginRegistration(
            id="dummy",
            version="0.1.0",
            operations=[
                plugin_pb2.PluginOp(
                    id="predict",
                    display_name="Predict (dummy)",
                    description="Return a small alanine structure.",
                    kind=plugin_pb2.OP_KIND_INVOKE,
                    creates_entities=True,
                    params=[
                        plugin_pb2.ParamSpec(
                            name="sequence",
                            display_name="Sequence",
                            description="Amino acid sequence (ignored by dummy).",
                            type=plugin_pb2.PARAM_TYPE_STRING,
                            default=make_param_value(""),
                        ),
                        plugin_pb2.ParamSpec(
                            name="num_recycles",
                            display_name="Num recycles",
                            description="Recycling iterations (ignored).",
                            type=plugin_pb2.PARAM_TYPE_INT,
                            default=make_param_value(3),
                            constraints=plugin_pb2.ParamConstraints(
                                int_range=plugin_pb2.IntRange(min=1, max=8),
                            ),
                        ),
                    ],
                ),
                plugin_pb2.PluginOp(
                    id="design",
                    display_name="Design (dummy)",
                    description="Return a single dummy design.",
                    kind=plugin_pb2.OP_KIND_INVOKE,
                    creates_entities=True,
                    params=[
                        plugin_pb2.ParamSpec(
                            name="length",
                            display_name="Length",
                            description="\"min-max\", e.g. 50-100.",
                            type=plugin_pb2.PARAM_TYPE_STRING,
                            default=make_param_value("50-100"),
                        ),
                        plugin_pb2.ParamSpec(
                            name="contig",
                            display_name="Contig",
                            description="Contig string (ignored).",
                            type=plugin_pb2.PARAM_TYPE_STRING,
                            default=make_param_value(""),
                        ),
                        plugin_pb2.ParamSpec(
                            name="num_designs",
                            display_name="Num designs",
                            type=plugin_pb2.PARAM_TYPE_INT,
                            default=make_param_value(1),
                            constraints=plugin_pb2.ParamConstraints(
                                int_range=plugin_pb2.IntRange(min=1, max=16),
                            ),
                        ),
                        plugin_pb2.ParamSpec(
                            name="save_trajectories",
                            display_name="Save trajectories",
                            type=plugin_pb2.PARAM_TYPE_BOOL,
                            default=make_param_value(False),
                        ),
                    ],
                ),
                plugin_pb2.PluginOp(
                    id="stream_test",
                    display_name="Stream test (dummy)",
                    description="Emit a few poll frames then a final alanine.",
                    kind=plugin_pb2.OP_KIND_STREAM,
                    params=[],
                ),
            ],
            queries=[
                plugin_pb2.PluginQuery(
                    id="sequence_design",
                    display_name="Sequence Design (dummy)",
                    description=(
                        "Return fixed dummy candidate sequences. "
                        "User picks one; a separate apply_sequence op commits."
                    ),
                    params=[
                        plugin_pb2.ParamSpec(
                            name="temperature",
                            display_name="Temperature",
                            type=plugin_pb2.PARAM_TYPE_FLOAT,
                            default=make_param_value(0.1),
                            constraints=plugin_pb2.ParamConstraints(
                                float_range=plugin_pb2.FloatRange(min=0.0, max=2.0),
                            ),
                        ),
                        plugin_pb2.ParamSpec(
                            name="num_sequences",
                            display_name="Num sequences",
                            type=plugin_pb2.PARAM_TYPE_INT,
                            default=make_param_value(1),
                            constraints=plugin_pb2.ParamConstraints(
                                int_range=plugin_pb2.IntRange(min=1, max=8),
                            ),
                        ),
                    ],
                ),
            ],
        )

    def invoke(
        self,
        session: int,
        op: str,
        context: DispatchContext,
        params: dict[str, Any],
    ) -> bytes:
        if op == "predict":
            sequence = params.get("sequence", "")
            num_recycles = params.get("num_recycles", 3)
            logger.info(
                "predict: sequence=%r, num_recycles=%d", sequence, num_recycles
            )
            return _ALANINE_PDB.encode("utf-8")

        if op == "design":
            length = params.get("length", "")
            contig = params.get("contig", "")
            num_designs = params.get("num_designs", 1)
            save_trajectories = params.get("save_trajectories", False)
            logger.info(
                "design: length=%r contig=%r num_designs=%d save_traj=%s",
                length,
                contig,
                num_designs,
                save_trajectories,
            )
            return _ALANINE_PDB.encode("utf-8")

        raise ValueError(f"Unknown op: {op!r}")

    def query(
        self,
        session: int,
        query: str,
        context: DispatchContext,
        params: dict[str, Any],
    ) -> bytes:
        if query == "sequence_design":
            temperature = params.get("temperature", 0.1)
            num_sequences = params.get("num_sequences", 1)
            logger.info(
                "sequence_design: temperature=%.2f num_sequences=%d",
                temperature,
                num_sequences,
            )
            sequences = ["AAA", "CCC", "GGG", "TTT"][:num_sequences]
            scores = [0.5, 0.6, 0.55, 0.45][:num_sequences]
            lines = [f"{s}\t{sc:.3f}" for s, sc in zip(sequences, scores)]
            return "\n".join(lines).encode("utf-8")

        raise ValueError(f"Unknown query: {query!r}")

    # Streaming op: scripted poll sequence, no background thread needed.

    def start_stream(
        self,
        session: int,
        op: str,
        context: DispatchContext,
        params: dict[str, Any],
        request_id: int,
    ) -> None:
        if op != "stream_test":
            raise ValueError(f"Unknown stream op: {op!r}")
        # Exercise the bound DispatchContext receive path: focus + the two
        # residue-ref lists arrive as native types from the host.
        logger.info(
            "start_stream: rid=%d focus=%s selection=%d designable=%d",
            request_id,
            context.focused_entity_id,
            len(context.selection),
            len(context.designable),
        )
        self._streams[request_id] = {"polls": 0, "cancelled": False}

    def poll_stream(self, request_id: int) -> PollOutcome:
        state = self._streams.get(request_id)
        if state is None:
            return PollOutcome.error(
                "STREAM_UNKNOWN",
                f"no stream for request_id {request_id}",
                {},
            )
        if state["cancelled"]:
            return PollOutcome.cancelled(_alanine_assembly_bytes())

        polls = state["polls"]
        state["polls"] = polls + 1
        if polls < 2:
            return PollOutcome.pending(progress=0.25 * (polls + 1), stage="working")
        if polls == 2:
            return PollOutcome.checkpoint(
                latest_assembly=_alanine_assembly_bytes(),
                progress=0.75,
                stage="checkpoint",
            )
        return PollOutcome.final_(_alanine_assembly_bytes())

    def cancel_stream(self, request_id: int) -> None:
        state = self._streams.get(request_id)
        if state is not None:
            state["cancelled"] = True
        logger.info("cancel_stream: rid=%d", request_id)
