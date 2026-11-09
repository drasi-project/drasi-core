"""Regression tests for the suite's measurable envelope/row/branch assertions."""

import copy
import unittest

from verify_scenarios import check, total
from verify_telemetry import sample
from verify_trace import verify


class ScenarioVerificationTests(unittest.TestCase):
    def setUp(self):
        self.state = {
            "config": {"scenario": "latency", "source": "http", "metrics": True, "metric_exclusion": "", "slow_delay_ms": 300},
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
            "config": {"scenario": "postgres", "source": "postgres", "metrics": True, "metric_exclusion": ""},
            "baseline_metrics": "", "paused": False,
            "since_reset": {"postgres-output": {"envelopes": 3, "rows": 3}},
        }
        metrics = 'drasi_query_events_processed{component_id="postgres-query",query_id="postgres-query",phase="steady"} 3'
        check(state, metrics, "actions")
        with self.assertRaisesRegex(ValueError, "query input envelope"):
            check(state, metrics.replace("postgres-query", "http-query"), "actions")


class TraceProfileTests(unittest.TestCase):
    def trace(self):
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
            span("component.process", 2, 1, component_id="http-query", input_count=1, output_count=1),
            span("sink.handle", 3, 2, component_id="http-output", component_kind="reaction", completion="handled", input_count=1, change_count=1),
        ]
        resource = {"attributes": [{"key": "service.name", "value": {"stringValue": "drasi-observability-demo"}}]}
        return {"result": {"resourceSpans": [{"resource": resource, "scopeSpans": [{"spans": spans}]}]}}, trace_id

    def test_basic_and_persistence_have_basic_not_debug_spans(self):
        data, trace_id = self.trace()
        for profile in ["basic", "persistence"]:
            with self.subTest(profile=profile):
                self.assertEqual(verify(data, "http", trace_id, profile, "controls"), 3)
        with self.assertRaisesRegex(ValueError, "span counts"):
            verify(data, "http", trace_id, "debug", "controls")

    def test_reaction_inherits_query_output_context(self):
        data, trace_id = self.trace()
        data["result"]["resourceSpans"][0]["scopeSpans"][0]["spans"][-1]["parentSpanId"] = "1".zfill(16)
        with self.assertRaisesRegex(ValueError, "query output context"):
            verify(data, "http", trace_id, "basic", "controls")

    def test_exported_service_name_is_verified(self):
        data, trace_id = self.trace()
        data["result"]["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"] = "old-service"
        with self.assertRaisesRegex(ValueError, "service.name"):
            verify(data, "http", trace_id, "basic", "controls")


if __name__ == "__main__":
    unittest.main()
