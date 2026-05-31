"""
Test program for DAP integration tests.

Exercises a broad range of DAP features:
  - Launch with stopOnEntry
  - Line breakpoints (plain + conditional)
  - Continue
  - Step over (next)
  - Step in (stepIn)
  - Step out (stepOut)
  - Stack trace inspection
  - Variable inspection (locals, globals, complex types)
  - Expression evaluation
  - Thread listing
  - Stdout output capture

Intended to be run with debugpy or any Python-capable DAP adapter.
"""


class Counter:
    """Simple class to exercise variable inspection of objects."""

    def __init__(self, start: int = 0):
        self.value = start
        self.label = "counter"

    def increment(self) -> int:
        self.value += 1
        return self.value


def factorial(n: int) -> int:
    """Recursive function for deeper stack traces."""
    if n <= 1:
        return 1
    return n * factorial(n - 1)


def compute_stats(numbers):
    """Process a list — exercise iteration and dict construction."""
    total = sum(numbers)
    count = len(numbers)
    avg = total / count if count else 0.0
    return {
        "sum": total,
        "count": count,
        "average": avg,
    }


def greet(name: str, greeting: str = "Hello") -> str:
    """Called from main — exercise stepping over a sub-call."""
    return f"{greeting}, {name}!"


def process_items(items):
    """Loop with a conditional — exercise conditional breakpoints."""
    results = []
    for item in items:
        doubled = item * 2          # conditional bp: item > 10
        results.append(doubled)
    return results


def outer():
    """Wrapper that calls middle — exercise step_out."""
    result = middle(5)
    return result * 2


def middle(x):
    """Calls inner — exercise step_in."""
    y = x + 3
    z = inner(y)
    return z + 1


def inner(x):
    """Leaf function — inspect locals here."""
    square = x * x
    return square


def main():
    # --- basic types: variables to inspect ---
    text = "Hello, DAP!"
    number = 42
    pi = 3.14159
    flag = True
    items = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 15, 20]
    mapping = {"key_a": 100, "key_b": 200}
    counter = Counter(start=10)

    print(f"text = {text}")
    print(f"number = {number}")

    # --- function call: exercise step_in ---
    greeting = greet("World")        # bp: line 86 (step_in friendly)
    print(greeting)

    # --- loop: exercise step_over within loop + conditional bp ---
    doubled = process_items(items)   # bp: line 89
    print(f"doubled = {doubled}")

    # --- recursion: deeper stack ---
    fact = factorial(5)              # bp: line 93
    print(f"factorial(5) = {fact}")

    # --- object inspection ---
    counter.increment()
    counter.increment()
    print(f"counter.value = {counter.value}")

    # --- dict/list inspection ---
    stats = compute_stats(doubled)   # bp: line 101
    print(f"stats = {stats}")

    # --- nested calls: step_in → step_out ---
    outer_result = outer()           # bp: line 105
    print(f"outer_result = {outer_result}")

    # --- direct expression evaluation targets ---
    x = 10                           # bp: line 109
    y = 20
    z = x + y
    s = f"sum: {z}"
    print(s)


if __name__ == "__main__":
    main()
