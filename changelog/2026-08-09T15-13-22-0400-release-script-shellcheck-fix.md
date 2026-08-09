# Release script ShellCheck fix

- Split command substitutions from `readonly` declarations so failures remain visible under strict Bash mode.
- Confirmed the release validator, workflow contract, and focused ShellCheck gate pass.
