"""Check a bench bucket before a run (plan 0024).

Usage: gcloud storage buckets describe gs://B --format=json |
           python3 check-bucket.py lifecycle.json REGION

Exits 0 if the bucket is in REGION and its lifecycle configuration contains
every rule of lifecycle.json; 1 otherwise. The rules are compared as
(action, age, prefixes) tuples, not as text: the API adds fields and uses
its own key order. A Delete rule that can remove objects under
"peppydb-bench/" before the required age also fails the check. Rules that
do not touch "peppydb-bench/" produce a warning only.
"""

import json
import sys

OUR_PREFIX = "peppydb-bench/"


def rules_of(config):
    if isinstance(config, dict):
        rules = config.get("rule", config.get("rules", []))
        return rules if isinstance(rules, list) else []
    return []


def describe(rule):
    action = rule.get("action", {}).get("type")
    condition = dict(rule.get("condition", {}))
    age = condition.pop("age", None)
    prefixes = condition.pop("matchesPrefix", None) or condition.pop("matches_prefix", None) or []
    return action, (None if age is None else int(age)), tuple(prefixes), condition


def overlaps(prefix, other):
    return prefix.startswith(other) or other.startswith(prefix)


def main():
    wanted = [describe(rule) for rule in rules_of(json.load(open(sys.argv[1])))]
    region = sys.argv[2].upper()
    bucket = json.load(sys.stdin)
    problems = []
    warnings = []

    location = str(bucket.get("location", "")).upper()
    if location != region:
        problems.append(f"the bucket is in {location or 'an unknown location'}, not {region}")

    config = bucket.get("lifecycle_config", bucket.get("lifecycle", bucket.get("lifecycleConfig", {})))
    actual = [describe(rule) for rule in rules_of(config)]

    for action, age, prefixes, _ in wanted:
        if not any(a == action and g == age and p == prefixes and not c for a, g, p, c in actual):
            problems.append(f"missing rule: {action} after {age} days under {', '.join(prefixes)}")

    for action, age, prefixes, condition in actual:
        if (action, age, prefixes, condition) in [(a, g, p, {}) for a, g, p, _ in wanted]:
            continue
        touched = [OUR_PREFIX] if not prefixes else [p for p in prefixes if overlaps(p, OUR_PREFIX)]
        if not touched:
            warnings.append(f"unrelated rule: {action} after {age} days under {', '.join(prefixes)}")
            continue
        if action != "Delete":
            warnings.append(f"rule {action} also applies under {', '.join(touched)}")
            continue
        # Ages of the wanted rules for the objects this rule can reach; it
        # deletes early if it is younger than any of them.
        required = [g for a, g, p, _ in wanted if any(overlaps(t, w) for t in touched for w in p)]
        if not required:
            warnings.append(f"rule {action} under {', '.join(prefixes)} touches no bench objects")
            continue
        if age is None or age < max(required):
            problems.append(
                f"a Delete rule ({'no age' if age is None else f'{age} days'}, "
                f"{', '.join(prefixes) or 'every object'}) can delete peppydb-bench objects early"
            )

    for warning in warnings:
        print(f"check-bucket: warning: {warning}", file=sys.stderr)
    for problem in problems:
        print(f"check-bucket: {problem}", file=sys.stderr)
    if problems:
        print(
            "check-bucket: apply the rules with: gcloud storage buckets update "
            f"gs://BUCKET --lifecycle-file={sys.argv[1]}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
