// 评审对象的测试：确认正常路径可用。
//
// 这些测试全部用已知存在的订单，忽略的那处错误不会被触发，因此
// 在破损基线上编译通过、测试也通过。评审任务考的是模型能不能
// 看出 FetchLines 里被吞掉的错误返回，而不是这些测试本身。
package service

import (
	"testing"
)

func TestFetchLinesHappyPath(t *testing.T) {
	s := NewService(NewMemoryStore())
	s.store.Save(Order{ID: "a", Line: "第一行"})
	s.store.Save(Order{ID: "b", Line: "第二行"})

	got := s.FetchLines([]string{"a", "b"})
	want := []string{"第一行", "第二行"}
	if len(got) != len(want) {
		t.Fatalf("行数 = %d, 期望 %d", len(got), len(want))
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("第 %d 行 = %q, 期望 %q", i, got[i], want[i])
		}
	}
}

func TestMissingOrderYieldsEmptyLine(t *testing.T) {
	// 缺失订单被 Load 报错，但 FetchLines 忽略错误，行项落回零值。
	s := NewService(NewMemoryStore())
	s.store.Save(Order{ID: "a", Line: "第一行"})

	got := s.FetchLines([]string{"a", "缺失"})
	if got[1] != "" {
		t.Fatalf("缺失订单的行项应为空串, 实际 %q", got[1])
	}
}
