"""Regression tests for the suite's measurable envelope/row/branch assertions."""

import copy
import unittest

from verify_scenarios import check, total
from verify_telemetry import sample
from verify_trace import text, verify


class ScenarioVerificationTests(unittest.TestCase):
    def setUp(self):
        self.state = {
            "config": {"scenario": "latency", "source": "http", "transformer": False, "metrics": True, "metric_exclusion": "", "slow_delay_ms": 300},
            "baseline_metrics": "",
            "paused": True,
            "since_reset": {
                "http-fast": {"envelopes": 1, "rows": 10},
                "http-slow": {"envelopes": 0, "rows": 0},
            },
        }
        self.metrics = '\n'.join([
            'drasi_query_events_processed{component_id="http-query",query_id="http-query",phase="steady"} 1',
            'drasi_query_results_emitted{component_id="http-query",query_id="http-query",change_kind="added"} 10',
            'drasi_pipeline_end_to_end_duration_count{component_id="http-fast",query_id="http-query",phase="steady"} 1',
        ])

    def test_valid_paused_branch(self):
        check(self.state, self.metrics, "paused-seed")

    def test_rows_are_not_envelopes(self):
        metrics = self.metrics.replace('phase="steady"} 1', 'phase="steady"} 10')
        with self.assertRaisesRegex(ValueError, "latency must observe envelopes"):
            check(self.state, metrics, "paused-seed")

    def test_acceptance_is_not_handling(self):
        metrics = self.metrics + '\ndrasi_pipeline_end_to_end_duration_count{component_id="http-slow",query_id="http-query",phase="steady"} 1'
        with self.assertRaisesRegex(ValueError, "latency must observe envelopes"):
            check(self.state, metrics, "paused-seed")

    def test_wrong_query_attribution_fails(self):
        with self.assertRaises(ValueError):
            check(self.state, self.metrics.replace('query_id="http-query"', 'query_id="postgres-query"'), "paused-seed")

    def test_wrong_row_count_fails(self):
        state = copy.deepcopy(self.state)
        state["since_reset"]["http-fast"]["rows"] = 1
        with self.assertRaisesRegex(ValueError, "ten rows"):
            check(state, self.metrics, "paused-seed")

    def test_exact_metric_and_label_matching(self):
        metrics = self.metrics + '\ndrasi_pipeline_end_to_end_duration_count{component_id="http-fast-other",query_id="http-query",phase="steady"} 99'
        self.assertEqual(total(metrics, "drasi_pipeline_end_to_end_duration_count", component_id="http-fast", query_id="http-query"), 1)
        self.assertEqual(total(metrics, "drasi_pipeline_end_to_end_duration"), 0)

    def test_registered_zero_phases_do_not_duplicate_work(self):
        metrics = '\n'.join([
            'drasi_query_events_processed{component_id="http-query",phase="steady"} 1',
            'drasi_query_events_processed{component_id="http-query",phase="bootstrap"} 0',
        ])
        self.assertEqual(sample(metrics, "drasi_query_events_processed", "http-query", aggregate=True), 1)
        with self.assertRaises(ValueError):
            sample(metrics, "drasi_query_events_processed", "http-query")

    def test_optional_sql_checks_use_postgres_not_http(self):
        state = {
            "config": {"scenario": "postgres", "source": "postgres", "transformer": False, "metrics": True, "metric_exclusion": ""},
            "baseline_metrics": "", "paused": False,
            "since_reset": {"postgres-output": {"envelopes": 3, "rows": 3}},
        }
        metrics = 'drasi_query_events_processed{component_id="postgres-query",query_id="postgres-query",phase="steady"} 3'
        check(state, metrics, "actions")
        with self.assertRaisesRegex(ValueError, "query input envelope"):
            check(state, metrics.replace("postgres-query", "http-query"), "actions")

    def test_transformer_actions_require_real_input_and_output_counts(self):
        state = {
            "config": {"scenario": "follow", "source": "http", "transformer": True, "metrics": True, "metric_exclusion": ""},
            "baseline_metrics": "", "paused": False,
            "since_reset": {"http-output": {"envelopes": 3, "rows": 3}},
        }
        metrics = '\n'.join([
            'drasi_query_events_processed{component_id="http-query",query_id="http-query",phase="steady"} 3',
            'drasi_component_inputs_processed{component_id="http-transform",completion="transformed"} 3',
            'drasi_component_outputs_emitted{component_id="http-transform"} 3',
        ])
        check(state, metrics, "actions")
        for instrument in ["drasi_component_inputs_processed", "drasi_component_outputs_emitted"]:
            with self.subTest(instrument=instrument), self.assertRaisesRegex(ValueError, "transformer .* envelope count"):
                check(state, "\n".join(line for line in metrics.splitlines() if not line.startswith(instrument)), "actions")


class TraceProfileTests(unittest.TestCase):
    def trace(self, transformer=True, debug=False):
        trace_id = "1" * 32
        def span(name, identifier, parent, **fields):
            result = {
                "name": name, "traceId": trace_id, "spanId": str(identifier).zfill(16),
                "startTimeUnixNano": "1", "endTimeUnixNano": "2",
                "attributes": [
                    {"key": key, "value": {"intValue": str(value)} if isinstance(value, int) else {"stringValue": value}}
                    for key, value in {"outcome": "success", **fields}.items()
                ],
            }
            if parent:
                result["parentSpanId"] = str(parent).zfill(16)
            return result
        spans = [
            span("source.publish", 1, None, component_id="http", output_count=1),
            span("query.process", 2, 1, component_id="http-query", input_count=1, output_count=1),
            span("sink.handle", 3, 2, component_id="http-output", component_kind="reaction", completion="handled", input_count=1, change_count=1),
        ]
        if transformer:
            spans.append(span("transformer.process", 4, 1, component_id="http-transform", component_kind="transformer", implementation="drasi.standard/arithmetic", input_count=1, output_count=1))
        if debug:
            spans.extend([
                span("query.evaluate", 5, 2, change_count=1, result_change_count=1),
                span("query.commit", 6, 2),
                span("pipe.send", 7, 1, component_id="http"),
                span("pipe.send", 8, 2, component_id="http-query"),
                span("query.confirm_outputs", 9, 2, component_id="http-query"),
                span("query.prepare_input", 12, 2, component_id="http-query"),
                span("query.prepare_output", 13, 2, component_id="http-query"),
                span("query.finalize", 14, 2, component_id="http-query"),
            ])
            timings = {
                "query.process": (1, 100),
                "query.prepare_input": (2, 10),
                "query.evaluate": (20, 30),
                "query.prepare_output": (40, 50),
                "query.commit": (60, 65),
                "query.finalize": (70, 80),
            }
            for observed in spans:
                if observed["name"] in timings:
                    start, end = timings[observed["name"]]
                    observed["startTimeUnixNano"] = str(start)
                    observed["endTimeUnixNano"] = str(end)
            if transformer:
                spans.extend([
                    span("pipe.send", 10, 1, component_id="http-transform"),
                    span("transformer.confirm_outputs", 11, 4, component_id="http-transform"),
                ])
        resource = {"attributes": [{"key": "service.name", "value": {"stringValue": "drasi-observability-demo"}}]}
        return {"result": {"resourceSpans": [{"resource": resource, "scopeSpans": [{"spans": spans}]}]}}, trace_id

    def test_basic_and_persistence_have_basic_not_debug_spans(self):
        data, trace_id = self.trace()
        for profile in ["basic", "persistence"]:
            with self.subTest(profile=profile):
                self.assertEqual(verify(data, "http", trace_id, profile, "controls"), 4)
        with self.assertRaisesRegex(ValueError, "span counts"):
            verify(data, "http", trace_id, "debug", "controls")

    def test_reaction_inherits_query_output_context(self):
        data, trace_id = self.trace()
        next(span for span in data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
             if span["name"] == "sink.handle")["parentSpanId"] = "1".zfill(16)
        with self.assertRaisesRegex(ValueError, "query output context"):
            verify(data, "http", trace_id, "basic", "controls")

    def test_generic_processing_name_is_rejected_for_query(self):
        data, trace_id = self.trace()
        data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"][1]["name"] = "component.process"
        with self.assertRaisesRegex(ValueError, "span counts"):
            verify(data, "http", trace_id, "basic")

    def test_exported_service_name_is_verified(self):
        data, trace_id = self.trace()
        data["result"]["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"] = "old-service"
        with self.assertRaisesRegex(ValueError, "service.name"):
            verify(data, "http", trace_id, "basic", "controls")

    def test_incoming_trace_preserves_remote_parent_and_new_local_spans(self):
        data, trace_id = self.trace()
        publication = data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0]
        parent = "abcdef0123456789"
        publication["parentSpanId"] = parent
        self.assertEqual(verify(data, "http", trace_id, "basic", "follow", parent), 4)
        with self.assertRaisesRegex(ValueError, "trace root"):
            verify(data, "http", trace_id, "basic")
        with self.assertRaisesRegex(ValueError, "incoming parent"):
            verify(data, "http", trace_id, "basic", "follow", "f" * 16)
        publication["parentSpanId"] = publication["spanId"]
        with self.assertRaisesRegex(ValueError, "new local span IDs"):
            verify(data, "http", trace_id, "basic", "follow", publication["spanId"])

    def test_transformer_trace_has_separate_generic_and_query_processing(self):
        for profile in ["basic", "persistence", "debug"]:
            with self.subTest(profile=profile):
                data, trace_id = self.trace(transformer=True, debug=profile == "debug")
                self.assertEqual(
                    verify(data, "http", trace_id, profile, "follow"),
                    14 if profile == "debug" else 4,
                )

    def test_transformer_does_not_rewrite_the_querys_input_parent(self):
        data, trace_id = self.trace(transformer=True, debug=True)
        spans = data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
        next(span for span in spans if span["name"] == "query.process")["parentSpanId"] = "4".zfill(16)
        with self.assertRaisesRegex(ValueError, "query parent is not publication"):
            verify(data, "http", trace_id, "debug", "follow")

    def test_transformer_confirmation_has_its_own_processing_parent(self):
        data, trace_id = self.trace(transformer=True, debug=True)
        spans = data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
        next(span for span in spans if span["name"] == "transformer.confirm_outputs" and
             text(span, "component_id") == "http-transform")["parentSpanId"] = "2".zfill(16)
        with self.assertRaisesRegex(ValueError, "transformer confirmation parent"):
            verify(data, "http", trace_id, "debug", "follow")

    def test_query_confirmation_has_its_own_processing_parent(self):
        data, trace_id = self.trace(debug=True)
        spans = data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
        next(span for span in spans if span["name"] == "query.confirm_outputs")["parentSpanId"] = "4".zfill(16)
        with self.assertRaisesRegex(ValueError, "query confirmation parent"):
            verify(data, "http", trace_id, "debug")

    def test_generic_confirmation_name_is_rejected_for_query_and_transformer(self):
        for name in ["query.confirm_outputs", "transformer.confirm_outputs"]:
            with self.subTest(name=name):
                data, trace_id = self.trace(debug=True)
                spans = data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
                next(span for span in spans if span["name"] == name)["name"] = "component.confirm_outputs"
                with self.assertRaisesRegex(ValueError, "span counts"):
                    verify(data, "http", trace_id, "debug")

    def test_transformer_uses_its_own_span_name_and_registered_implementation(self):
        data, trace_id = self.trace()
        transforming = next(span for span in data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
                            if span["name"] == "transformer.process")
        transforming["name"] = "component.process"
        with self.assertRaisesRegex(ValueError, "span counts"):
            verify(data, "http", trace_id, "basic")
        transforming["name"] = "transformer.process"
        next(field for field in transforming["attributes"] if field["key"] == "implementation")["value"]["stringValue"] = "other/transformer"
        with self.assertRaisesRegex(ValueError, "transformer implementation"):
            verify(data, "http", trace_id, "basic")

    def test_postgres_keeps_its_direct_pipeline_without_a_transformer(self):
        data, trace_id = self.trace(transformer=False, debug=True)
        for span in data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]:
            for entry in span["attributes"]:
                if entry["key"] == "component_id":
                    entry["value"]["stringValue"] = entry["value"]["stringValue"].replace("http", "postgres")
        self.assertEqual(verify(data, "postgres", trace_id, "debug", "postgres"), 11)

    def test_query_detail_stays_within_processing_and_preserves_phase_order(self):
        data, trace_id = self.trace(debug=True)
        spans = data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
        finalize = next(span for span in spans if span["name"] == "query.finalize")
        finalize["endTimeUnixNano"] = "101"
        with self.assertRaisesRegex(ValueError, "inside query.process"):
            verify(data, "http", trace_id, "debug")
        finalize["endTimeUnixNano"] = "80"
        finalize["startTimeUnixNano"] = "61"
        with self.assertRaisesRegex(ValueError, "phase boundaries"):
            verify(data, "http", trace_id, "debug")


if __name__ == "__main__":
    unittest.main()
