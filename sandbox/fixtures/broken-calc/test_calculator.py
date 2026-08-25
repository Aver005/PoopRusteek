import unittest

from calculator import average, largest, total


class TestCalculator(unittest.TestCase):
    def test_total(self):
        self.assertEqual(total([1, 2, 3]), 6)

    def test_average(self):
        self.assertEqual(average([2, 4]), 3)

    def test_average_of_nothing_is_zero(self):
        self.assertEqual(average([]), 0)

    def test_largest(self):
        self.assertEqual(largest([3, 9, 4]), 9)


if __name__ == "__main__":
    unittest.main()
