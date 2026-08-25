"""Small numeric helpers."""


def total(values):
    return sum(values)


def average(values):
    return sum(values) / len(values)


def largest(values):
    biggest = values[0]
    for value in values:
        if value > biggest:
            biggest = value
    return biggest
