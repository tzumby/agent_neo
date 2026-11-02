package main

import "testing"

func TestWhoGetsIn(t *testing.T) {
	logins, tags := []string{"me@example.com"}, []string{"tag:user-alex"}
	cases := []struct {
		name      string
		login     string
		nodeTags  []string
		allowed   []string
		allowTags []string
		want      bool
	}{
		{"no lists: the tailnet policy decides", "anyone@example.com", nil, nil, nil, true},
		{"a login on the list", "me@example.com", nil, logins, nil, true},
		{"a login not on it", "other@example.com", nil, logins, nil, false},
		{"the tester's tagged device", "tagged-devices", []string{"tag:user-alex"}, nil, tags, true},
		{"another tester's device", "tagged-devices", []string{"tag:user-sam"}, nil, tags, false},
		{"a tagged device's login isn't a login", "tagged-devices", []string{"tag:user-sam"}, []string{"tagged-devices"}[:0], tags, false},
		{"an untagged device with only tags allowed", "me@example.com", nil, nil, tags, false},
		{"either list lets in", "me@example.com", nil, logins, tags, true},
	}
	for _, c := range cases {
		if got := admits(c.login, c.nodeTags, c.allowed, c.allowTags); got != c.want {
			t.Errorf("%s: admits = %v, want %v", c.name, got, c.want)
		}
	}
	if got := list(" a, ,b,"); len(got) != 2 || got[0] != "a" || got[1] != "b" {
		t.Errorf("list = %q", got)
	}
}
