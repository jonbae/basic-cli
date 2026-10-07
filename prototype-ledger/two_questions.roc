## PROTOTYPE, throwaway: the two-question app for the effect-ledger supervisor wiring.
app [main!] { pf: platform "../platform/main.roc" }

import pf.OsStr
import pf.Stdin
import pf.Stdout

main! : List(OsStr) => Try({}, _)
main! = |_args| {
	Stdout.line!("Name?")?
	name = Stdin.line!() ? |_| MissingName

	Stdout.line!("Age?")?
	age = Stdin.line!() ? |_| MissingAge

	Stdout.line!("Hi ${name}, ${age} is a fine age.")?
	Ok({})
}
