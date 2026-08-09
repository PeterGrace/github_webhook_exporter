# Task 5 Round 1

- Replaced the workflow validator's PyYAML dependency with `yq` plus Python stdlib JSON parsing.
- Tightened the GitHub Actions contract to inspect exact `.jobs.validate.steps` entries and reject early uploads or echoed command text.
- Renamed the workflow job to `validate` to match the parsed structure.
- Verified installer outputs and added negative temp-workflow checks for early upload and echoed command text.
