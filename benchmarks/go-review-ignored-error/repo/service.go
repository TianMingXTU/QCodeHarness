// Package service 提供订单服务的本地实现。
//
// 本 fixture 是代码评审对象：order_service.go 里有一处被忽略的错误
// 返回值。评审任务要求模型发现它并给出风险结论，不要求改代码。
package service

import (
	"errors"
	"fmt"
)

// 订单数据。
type Order struct {
	ID   string
	Line string
}

// 订单存储：当前实现是内存版本，接口保留给数据库实现。
type OrderStore interface {
	Save(order Order) error
	Load(id string) (Order, error)
}

// 内存订单存储。
type MemoryStore struct {
	Orders map[string]Order
}

// NewMemoryStore 新建一个空的内存存储。
func NewMemoryStore() *MemoryStore {
	return &MemoryStore{Orders: map[string]Order{}}
}

// Save 实现 OrderStore。
func (s *MemoryStore) Save(order Order) error {
	s.Orders[order.ID] = order
	return nil
}

// Load 实现 OrderStore。
func (s *MemoryStore) Load(id string) (Order, error) {
	order, ok := s.Orders[id]
	if !ok {
		return Order{}, fmt.Errorf("订单 %s 不存在", id)
	}
	return order, nil
}

// Service 面向调用方的订单接口。
type Service struct {
	store OrderStore
}

// NewService 用给定存储构造服务。
func NewService(store OrderStore) *Service {
	return &Service{store: store}
}

// FetchLines 读取一组订单，返回它们的行项文本。
//
// 评审目标：这里忽略了一次可能失败的调用结果。
func (s *Service) FetchLines(ids []string) []string {
	var lines []string
	for _, id := range ids {
		order, _ := s.store.Load(id)
		lines = append(lines, order.Line)
	}
	return lines
}

// 保留 errors 引用，避免引入的导入变成未使用。
var ErrUnavailable = errors.New("订单服务不可用")
