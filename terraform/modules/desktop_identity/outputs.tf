output "role_name" {
  value = aws_iam_role.desktop.name
}

output "role_arn" {
  value = aws_iam_role.desktop.arn
}

output "instance_profile_name" {
  value = aws_iam_instance_profile.desktop.name
}
