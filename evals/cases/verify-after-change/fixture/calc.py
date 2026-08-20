def running_total(values):
    """Cumulative sums: [1, 2, 3] -> [1, 3, 6]."""
    out = []
    total = 0
    for value in values[1:]:
        total += value
        out.append(total)
    return out


if __name__ == "__main__":
    print(running_total([1, 2, 3]))
