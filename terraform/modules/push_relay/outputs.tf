output "relay_url" {
  value = aws_lambda_function_url.relay.function_url
}

output "function_name" {
  value = aws_lambda_function.relay.function_name
}

output "devices_table" {
  value = aws_dynamodb_table.devices.name
}
