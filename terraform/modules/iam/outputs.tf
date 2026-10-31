output "desktop_role_name" {
  value = aws_iam_role.desktop.name
}

output "desktop_role_arn" {
  value = aws_iam_role.desktop.arn
}

output "desktop_instance_profile_name" {
  value = aws_iam_instance_profile.desktop.name
}

output "desktop_instance_profile_arn" {
  value = aws_iam_instance_profile.desktop.arn
}
