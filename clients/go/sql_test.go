// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package bigdb

import (
	"math"
	"strconv"
	"testing"
)

// These vectors are the ones in contrib/big-message/src/sql.rs's `mod tests`, carried over
// case for case. The point of copying rather than inventing is that the two clients are then
// held to each other, not merely each to itself.

func lit(t *testing.T, v any) string {
	t.Helper()
	s, err := Literal(v)
	if err != nil {
		t.Fatalf("Literal(%#v): %v", v, err)
	}
	return s
}

func TestAQuoteInAValueCannotEndTheStatement(t *testing.T) {
	for _, c := range []struct{ in, want string }{
		{"O'Brien", "'O''Brien'"},
		{"x'); DROP TABLE t; --", "'x''); DROP TABLE t; --'"},
		// The empty string is a key like any other, and it is not the absence of one.
		{"", "''"},
		// A run of quotes doubles every one of them, rather than the first.
		{"'''", "''''''''"},
	} {
		if got := lit(t, c.in); got != c.want {
			t.Errorf("Literal(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestAQuoteInANameCannotEndTheIdentifier(t *testing.T) {
	got, err := QuoteIdent(`we"ird`)
	if err != nil {
		t.Fatal(err)
	}
	if want := `"we""ird"`; got != want {
		t.Errorf("QuoteIdent = %q, want %q", got, want)
	}
}

func TestANameThatSpellsAKeywordIsStillAName(t *testing.T) {
	// The reason every identifier is quoted: unquoted, this is Tok::Word("values") and the
	// parser reads it as the keyword.
	got, err := QuoteIdent("values")
	if err != nil {
		t.Fatal(err)
	}
	if want := `"values"`; got != want {
		t.Errorf("QuoteIdent = %q, want %q", got, want)
	}
}

func TestAnEmptyNameIsRefused(t *testing.T) {
	if _, err := QuoteIdent(""); err == nil {
		t.Error("an empty identifier must not reach a statement")
	}
}

func TestAQualifiedTableIsQuotedInTwoPieces(t *testing.T) {
	for _, c := range []struct{ in, want string }{
		{"sales.orders", `"sales"."orders"`},
		{"orders", `"orders"`},
	} {
		got, err := QuoteTable(c.in)
		if err != nil {
			t.Fatal(err)
		}
		if got != c.want {
			t.Errorf("QuoteTable(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestTheRecordIDColumnIsRefusedHoweverItIsSpelled(t *testing.T) {
	for _, spelling := range []string{"_record_id", "_RECORD_ID", "_Record_Id"} {
		if err := CheckColumn(spelling); err == nil {
			t.Errorf("%s names the record id and must not be writable", spelling)
		}
	}
	if err := CheckColumn("record_id"); err != nil {
		t.Errorf("an ordinary column that merely reads like it: %v", err)
	}
	if err := CheckColumn("id"); err != nil {
		t.Errorf("`id` belongs to whoever is writing the table: %v", err)
	}
	if err := CheckColumn(""); err == nil {
		t.Error("a column with no name must be refused")
	}
}

func TestAFloatThatIsNotANumberIsRefusedBeforeItIsSent(t *testing.T) {
	for _, v := range []float64{math.NaN(), math.Inf(1), math.Inf(-1)} {
		if _, err := Literal(v); err == nil {
			t.Errorf("%v must not reach a statement", v)
		}
	}
}

func TestAFloatIsWrittenInTheOnlyNotationThisDialectHas(t *testing.T) {
	// Ordinary magnitudes come straight out of FormatFloat.
	for _, c := range []struct {
		in   float64
		want string
	}{
		{2.75, "2.75"},
		{-0.5, "-0.5"},
		// Go writes this "0" where Rust's {:?} writes "0.0". Both are numbers the lexer reads;
		// the assertion is on the value, not on the spelling.
		{0.0, "0"},
	} {
		if got := lit(t, c.in); got != c.want {
			t.Errorf("Literal(%v) = %q, want %q", c.in, got, c.want)
		}
	}

	// Rust's {:?} gives 1e-7, which the lexer cannot read, and sql.rs then searches for a fixed
	// spelling. Go's 'f' never produces an exponent, so there is nothing to search for - but the
	// property being asserted is identical.
	for _, v := range []float64{1e-7, 1e18} {
		got := lit(t, v)
		for i := 0; i < len(got); i++ {
			if got[i] == 'e' || got[i] == 'E' {
				t.Fatalf("%v rendered as %q, which still carries an exponent", v, got)
			}
		}
		back, err := strconv.ParseFloat(got, 64)
		if err != nil || back != v {
			t.Errorf("%v rendered as %q, which reads back as %v", v, got, back)
		}
	}
}

func TestAFloatWithNoSpellingIsRefusedRatherThanRounded(t *testing.T) {
	// 1e300 written out is three hundred and one digits, and units is a u64. Refusing is the
	// honest answer; rounding it to something that fits would be writing a different number
	// than the caller sent.
	if _, err := Literal(1e300); err == nil {
		t.Error("1e300 has no spelling here and must be refused, not rounded")
	}
}

func TestADecimalIsCheckedAgainstTheGrammarTheLexerHas(t *testing.T) {
	for _, c := range []struct{ in, want string }{
		{"12.50", "12.50"},
		{"-12.50", "-12.50"},
		{"0", "0"},
	} {
		if got := lit(t, Decimal(c.in)); got != c.want {
			t.Errorf("Literal(Decimal(%q)) = %q, want %q", c.in, got, c.want)
		}
	}
	for _, bad := range []string{"", ".5", "5.", "1e3", "1,5", "12.5.0", "abc", "-", "- 1", "+1"} {
		if _, err := Literal(Decimal(bad)); err == nil {
			t.Errorf("%q must not reach a statement", bad)
		}
	}
}

func TestANumberWithMoreDigitsThanALiteralHoldsIsRefused(t *testing.T) {
	// One past u64::MAX, which is where lex::number answers NumberTooLarge.
	if _, err := Literal(Decimal("18446744073709551616")); err == nil {
		t.Error("18446744073709551616 overflows units and must be refused")
	}
	// The same digits with a point in them are the same units, so the same refusal.
	if _, err := Literal(Decimal("1844674407370955161.6")); err == nil {
		t.Error("1844674407370955161.6 is the same units and must be refused")
	}
	// And just inside it is fine.
	if _, err := Literal(Decimal("18446744073709551615")); err != nil {
		t.Errorf("u64::MAX is a number this dialect reads: %v", err)
	}
	// A negative literal is an i64, so u64::MAX with a sign does not fit.
	if _, err := Literal(Decimal("-18446744073709551615")); err == nil {
		t.Error("a negative literal holds an i64 and must refuse this")
	}
	// 256 digits after the point is one past what scale holds.
	long := "0."
	for i := 0; i < 256; i++ {
		long += "1"
	}
	if _, err := Literal(Decimal(long)); err == nil {
		t.Error("a literal carries at most 255 digits after the point")
	}
}

func TestAKeyedValueCarriesItsMomentAndIsStillQuoted(t *testing.T) {
	for _, c := range []struct {
		in   Keyed
		want string
	}{
		{Keyed{"gb", 1_750_000_000}, "'gb@1750000000'"},
		// The server splits on the last @, so a key holding one survives the round trip.
		{Keyed{"a@b", 1}, "'a@b@1'"},
		// And a key holding a quote is doubled like any other string.
		{Keyed{"o'b", 1}, "'o''b@1'"},
	} {
		if got := lit(t, c.in); got != c.want {
			t.Errorf("Literal(%#v) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestABoolIsWrittenTheWayTheParserEatsIt(t *testing.T) {
	if got := lit(t, true); got != "TRUE" {
		t.Errorf("true = %q", got)
	}
	if got := lit(t, false); got != "FALSE" {
		t.Errorf("false = %q", got)
	}
}

func TestASignedValueKeepsItsSignAndAnUnsignedOneHasNone(t *testing.T) {
	if got := lit(t, int64(-1)); got != "-1" {
		t.Errorf("-1 = %q", got)
	}
	if got, want := lit(t, int64(math.MinInt64)), "-9223372036854775808"; got != want {
		t.Errorf("MinInt64 = %q, want %q", got, want)
	}
	if got, want := lit(t, uint64(math.MaxUint64)), "18446744073709551615"; got != want {
		t.Errorf("MaxUint64 = %q, want %q", got, want)
	}
}

func TestNilIsRefusedBecauseTheDialectHasNoNull(t *testing.T) {
	if _, err := Literal(nil); err == nil {
		t.Fatal("this dialect has no NULL literal, so nil must be refused")
	}
	var p *int
	// A typed nil pointer reaches the default branch rather than the nil case, and must still
	// be refused - it is the same absence wearing a type.
	if _, err := Literal(p); err == nil {
		t.Error("a nil pointer must be refused too")
	}
}

func TestBytesAreRefusedRatherThanTreatedAsText(t *testing.T) {
	if _, err := Literal([]byte("hello")); err == nil {
		t.Error("a byte string has no spelling here")
	}
}
