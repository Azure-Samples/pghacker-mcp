# Privacy

## Data Collection

The software may collect information about you and your use of the software and send it to Microsoft. Microsoft may use this information to provide services and improve our products and services. You may turn off the telemetry as described in the repository. There are also some features in the software that may enable you and Microsoft to collect data from users of your applications. If you use these features, you must comply with applicable law, including providing appropriate notices to users of your applications together with a copy of Microsoft's privacy statement. Our privacy statement is located at https://go.microsoft.com/fwlink/?LinkID=824704. You can learn more about data collection and use in the help documentation and our privacy statement. Your use of the software operates as your consent to these practices.

## Turning telemetry off

- **Deployed with this repository's Terraform (telemetry is on by default):** set `usage_enabled = false` in your `.tfvars` and run `terraform apply`. No Application Insights resource is created and the app gets no connection string, so nothing is sent.
- **Run any other way:** `npm start` never sends telemetry, and the binaries on their own send none unless `PGHACKER_USAGE_ENDPOINT` is set.

## Recording less

What users type into tools is recorded only if a deployment sets `usage_details_enabled = true` (`PGHACKER_USAGE_DETAILS`): the tools' arguments, such as SQL and search terms; error messages, which can quote them; and the parser's errors for SQL that `query` checks by its tokens. It's off by default. Without it, a call keeps which tool it was, its arguments' names and size, the values of arguments that are one of a fixed set of choices (such as a search mode, or true or false), its outcome and, for a failed `query`, PostgreSQL's error code. Feedback comments are recorded only with `feedback_comments_enabled = true`, whatever this setting.
