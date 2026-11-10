package client

import "testing"

// transientError 模拟一次可重试的瞬时故障，测试用它驱动 Do。
type transientError struct{}

func (transientError) Error() string { return "transient" }

var transient = transientError{}

// TestRetry 引用了 client.go 尚未实现的 Retry，因此破损 fixture 上
// 编译失败。补齐 Retry 后这条测试定义它的语义：
// 前 MaxAttempts-1 次失败、最后一次成功，调用次数恰为 2，返回 nil。
func TestRetry(t *testing.T) {
	calls := 0
	c := Client{
		MaxAttempts: 3,
		Do: func() error {
			calls++
			if calls < 2 {
				return transient
			}
			return nil
		},
	}
	if err := c.Retry(); err != nil {
		t.Fatalf("Retry() = %v, want nil", err)
	}
	if calls != 2 {
		t.Fatalf("Do called %d times, want 2", calls)
	}
}

// TestGetStillWorks 保证补齐 Retry 没有破坏既有的单次执行路径。
func TestGetStillWorks(t *testing.T) {
	c := Client{MaxAttempts: 1, Do: func() error { return nil }}
	if err := c.Get(); err != nil {
		t.Fatalf("Get() = %v, want nil", err)
	}
}
