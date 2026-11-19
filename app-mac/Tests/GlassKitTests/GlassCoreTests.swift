import XCTest
@testable import GlassKit

final class GlassCoreTests: XCTestCase {
    func testCoreLinksAndReportsVersions() {
        XCTAssertFalse(GlassCore.version.isEmpty)
        XCTAssertEqual(GlassCore.version.split(separator: ".").count, 3)
        XCTAssertEqual(GlassCore.protocolVersion, 0)
    }
}
