// Package client 把一次外部请求包装成带重试的调用。
//
// 当前只实现了单次执行的 Get。测试 client_test.go 引用了尚未实现的
// Retry 方法，因此 go test 在编译阶段就失败：补上 Retry 是本次任务。
package client

// Client 封装一次请求的发起方式与重试上限。
type Client struct {
	// MaxAttempts 是 Retry 最多执行的次数。
	MaxAttempts int
	// Do 是真正发起请求的入口，返回 nil 表示成功。
	Do func() error
}

// Get 执行一次请求，返回发起结果。
func (c *Client) Get() error {
	return c.Do()
}
