output "ssm_transfer_bucket" {
  value = aws_s3_bucket.ssm_transfer.bucket
}

output "ssm_transfer_bucket_arn" {
  value = aws_s3_bucket.ssm_transfer.arn
}
