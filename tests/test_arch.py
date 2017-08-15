import puddle.arch
import glob

def test_arch():
    tests = glob.glob('tests/arches/*')
    assert len(tests) > 0
    for test in tests:
        with open(test) as f:
            gstr = f.read()

        graph = puddle.arch.parse_arch(gstr)
        assert puddle.arch.pp_arch(graph) == gstr

