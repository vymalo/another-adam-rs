# Per-line discounts on invoices

Customers get discounts on single items, and today we have to fake them with a separate negative line, which the
validation refuses anyway. Add an optional discount to each line of the JSON description:

```json
{ "description": "Widget", "quantity": 2, "unitPrice": "12.50", "discount": "10%" }
```

Rules:

* `discount` is a rate written like `taxRate` (`"10%"`, `"12.5%"`), from 0% to 100%. A missing discount is 0%. A
  value that is not a rate, or above 100%, is refused with an `InvoiceError` whose message names the line
  (`line 2: ...`), like the other line errors.
* The discount is taken off its line: quantity times unit price, less the discount rounded half up to the cent (the
  same rounding as tax). The subtotal is the sum of the lines after their discounts, and tax is computed on that
  subtotal. `totals()` keeps returning `{ subtotal, tax, total }`.
* The rendering shows the discount on its line, before the line's total, and, when any line has a discount above 0%, a
  `Discounts:` row with the sum of what the discounts took off, just before `Subtotal:`. Lines and invoices without a
  discount (or with 0%) render exactly as they do now.

For the README's example with a 10% discount on the widgets:

```
Invoice 2026-041
  2 x Widget @ 12.50 EUR less 10% = 22.50 EUR
  1 x Setup @ 40.00 EUR = 40.00 EUR
Discounts: 2.50 EUR
Subtotal: 62.50 EUR
Tax (20%): 12.50 EUR
Total: 75.00 EUR
```

The CLI prints the same. Document the field in the README. Keep the existing tests passing (`npm test`).
