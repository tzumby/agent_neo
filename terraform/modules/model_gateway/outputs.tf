output "url" {
  value = aws_lambda_function_url.gateway.function_url
}

output "function_name" {
  value = aws_lambda_function.gateway.function_name
}

# Shared with the attested gateway (gatewayd), which keeps the same records.
output "usage_table_name" {
  value = aws_dynamodb_table.usage.name
}

output "usage_table_arn" {
  value = aws_dynamodb_table.usage.arn
}
