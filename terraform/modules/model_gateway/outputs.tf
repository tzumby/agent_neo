output "url" {
  value = aws_lambda_function_url.gateway.function_url
}

output "function_name" {
  value = aws_lambda_function.gateway.function_name
}
